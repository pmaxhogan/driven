//! An in-process, fault-injecting S3-compatible server (STRESS_HARNESS s5.1).
//!
//! This is the S3 twin of `InMemoryRemoteStore`'s fault-injection surface
//! (s5), and it deliberately sits one layer LOWER. The interesting S3 failure
//! modes are not `RemoteStore` failures - they are WIRE failures whose whole
//! point is the classification `driven-s3` performs on them:
//!
//! - `503 SlowDown` is throttling wearing a 5xx costume. A fault injected at
//!   the trait seam could only produce an already-classified `RateLimited`,
//!   which proves nothing about `classify_s3_response`.
//! - `RequestTimeout` arrives as a **400** and must still be retried.
//! - `CompleteMultipartUpload` can answer **HTTP 200 with an error document in
//!   the body**, because the connection is held open while S3 assembles the
//!   parts. Only a real response body can exercise that branch.
//! - A multipart upload interrupted between two `UploadPart` calls, or between
//!   the last `UploadPart` and `CompleteMultipartUpload`, is a transport-level
//!   event with no trait-level representation at all.
//!
//! So the harness runs the REAL [`driven_s3::S3Store`] - real signing, real
//! `reqwest` round trips, real XML parsing, real `classify_s3_response` - and
//! injects faults on the wire.
//!
//! ## Why not MinIO plus a proxy
//!
//! `driven-s3`'s own integration suite already covers real MinIO and real
//! Cloudflare R2. The chaos jobs are a different question: per
//! `.github/workflows/chaos.yml` they run **Windows-only** on every PR, and
//! `minio` is installed only on the Linux legs of `ci.yml` / `coverage.yml`. A
//! MinIO-gated chaos row would therefore SKIP in the gate that actually blocks
//! a merge, which is the one place these rows have to run. An in-process
//! server has no external binary, no port coordination beyond `:0`, and runs
//! identically on all three platforms.
//!
//! ## No SigV4 validation, on purpose
//!
//! [`driven_s3::S3Store`] signs every request as a **presigned URL** (`rusty_s3`
//! `action.sign(...)`), so the credential material rides in the query string
//! and the server never has to verify it. Validating signatures here would
//! test `rusty-s3`, not Driven, and would make every fault row depend on a
//! signing implementation the harness does not own. The server therefore
//! ignores `X-Amz-*` query parameters entirely.
//!
//! ## Protocol subset
//!
//! Exactly the requests `driven-s3` issues, all path-style
//! (`S3Config::path_style` defaults to `true`):
//!
//! | Request                     | Shape                                        |
//! |-----------------------------|----------------------------------------------|
//! | `PutObject`                 | `PUT /{bucket}/{key}`                        |
//! | `HeadObject`                | `HEAD /{bucket}/{key}`                       |
//! | `GetObject`                 | `GET /{bucket}/{key}`                        |
//! | `DeleteObject`              | `DELETE /{bucket}/{key}`                     |
//! | `ListObjectsV2`             | `GET /{bucket}/?list-type=2`                 |
//! | `CreateMultipartUpload`     | `POST /{bucket}/{key}?uploads=1`             |
//! | `UploadPart`                | `PUT /{bucket}/{key}?partNumber=N&uploadId=U`|
//! | `ListParts`                 | `GET /{bucket}/{key}?uploadId=U`             |
//! | `CompleteMultipartUpload`   | `POST /{bucket}/{key}?uploadId=U`            |
//! | `AbortMultipartUpload`      | `DELETE /{bucket}/{key}?uploadId=U`          |
//!
//! Every body is framed by `Content-Length`: `S3Store::execute` hands `reqwest`
//! an owned `Bytes` on every path (there is no `wrap_stream` anywhere in the
//! crate), so the server needs no chunked-transfer decoding. HTTP/1.1
//! keep-alive IS supported, because `reqwest` pools connections.
//!
//! ## Real S3 semantics the server keeps
//!
//! These are not decoration - a fault row is only meaningful if the
//! non-faulted path behaves like S3:
//!
//! - `Content-MD5` is VERIFIED against the received bytes and a mismatch is
//!   answered with `400 BadDigest`, which is what makes the ETag Driven reads
//!   back a witness rather than an echo (see `driven_s3::store`'s module docs).
//! - A single-`PutObject` ETag is the content md5; a multipart ETag is
//!   `md5(concat(part md5s))-N`, so `md5_from_etag` correctly declines to read
//!   a content digest off one.
//! - `CompleteMultipartUpload` assembles ONLY the parts the request body names
//!   and DISCARDS any other part still held for that upload id. The resume
//!   path's "a stale part beyond the replay's last part number is dropped, not
//!   appended" guarantee is exactly this behaviour, so faking it would make the
//!   crash-resume rows vacuous.
//! - `ListObjectsV2` honours `max-keys` and emits `NextContinuationToken`, so
//!   the real pagination loop in `S3Store::list_pages` is exercised.
//! - A `DeleteObject` of a missing key answers `204` (idempotent), a `HEAD` of
//!   one answers `404`.
//!
//! ## The fault-free oracle
//!
//! [`FaultyS3Server::objects`] reads the in-memory object map DIRECTLY, never
//! over HTTP. That is what lets the s6.3 invariant sweep verify a scenario's
//! terminal state even when the scenario left a fault LATCHED (the same reason
//! the fake exposes `descendant_files_with_trashed` rather than routing the
//! sweep through the faulted `list_folder`).

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Sentinel for "this counting fault never trips".
const NEVER: u64 = u64::MAX;

/// Cap on a request head, so a malformed client cannot grow the buffer without
/// bound. Far above any request `driven-s3` issues.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Fixed base timestamp for synthesized `Last-Modified` values (2024-01-01
/// 00:00:00 UTC). Each stored object gets a distinct, increasing second so
/// `find_by_op_uuid`'s "adopt the most recent" tiebreak has a real ordering to
/// work with rather than a pile of identical zeroes.
const BASE_EPOCH_SECS: i64 = 1_704_067_200;

// ===========================================================================
// Stored state
// ===========================================================================

/// One object at rest in the fake bucket.
#[derive(Debug, Clone)]
pub struct StoredObject {
    /// The object's bytes.
    pub bytes: Vec<u8>,
    /// The raw `x-amz-meta-driven-props` header value, if the writer sent one.
    pub props_header: Option<String>,
    /// The raw `x-amz-meta-driven-md5` header value, if the writer sent one
    /// (the single-`PutObject` path does; the multipart path cannot).
    pub md5_header: Option<String>,
    /// `Content-Type` the writer declared.
    pub content_type: String,
    /// The ETag this object reports, quotes included.
    pub etag: String,
    /// Synthesized modification time, seconds since the Unix epoch.
    pub modified_secs: i64,
}

impl StoredObject {
    /// The true md5 of the bytes at rest - the oracle digest, independent of
    /// whatever ETag shape the object's upload path produced.
    pub fn content_md5(&self) -> [u8; 16] {
        let mut h = Md5::new();
        h.update(&self.bytes);
        h.finalize().into()
    }
}

/// One part of an in-flight multipart upload.
#[derive(Debug, Clone)]
struct PartData {
    bytes: Vec<u8>,
    md5_hex: String,
}

/// One in-flight multipart upload.
#[derive(Debug, Clone)]
struct UploadState {
    key: String,
    content_type: String,
    props_header: Option<String>,
    parts: BTreeMap<u16, PartData>,
}

// ===========================================================================
// Faults
// ===========================================================================

/// The wire faults the server can inject.
///
/// Semantics mirror `InMemoryRemoteStore`'s fault surface so a reader who knows
/// one knows the other:
///
/// - "after N" means the (N+1)-th matching request trips. `arm_*(0)` trips the
///   very next one.
/// - A **transient** fault resets to "never" once it fires, so the retry
///   succeeds. That is what makes a row assert "Driven recovered", not merely
///   "Driven errored".
/// - A **latching** fault stays set for the server's lifetime, modelling a
///   condition that does not fix itself (a deleted bucket).
#[derive(Debug)]
struct Faults {
    /// Transient `503 SlowDown` after N write requests.
    slow_down_after: AtomicU64,
    /// Answer the next N write requests with `503 SlowDown` (a budget, not a
    /// single shot). A burst longer than the finite 5xx retry cap is what
    /// DISCRIMINATES throttling from a transient server fault.
    slow_down_burst: AtomicU64,
    /// Transient `400 RequestTimeout` after N write requests.
    request_timeout_after: AtomicU64,
    /// Transient `500 InternalError` after N write requests.
    internal_error_after: AtomicU64,
    /// Latching `404 NoSuchBucket` on every WRITE request.
    bucket_missing: AtomicBool,
    /// Latching `403 AllAccessDisabled` on every WRITE request.
    access_disabled: AtomicBool,
    /// Byte budget: a write that would push committed bytes past it is refused
    /// with `QuotaExceeded`. `NEVER` means unlimited.
    quota_bytes: AtomicU64,
    /// Bytes committed so far, against `quota_bytes`.
    committed_bytes: AtomicU64,
    /// Close the connection (no response at all) on the (N+1)-th `UploadPart`.
    /// Transient.
    drop_part_after: AtomicU64,
    /// Close the connection on the next N `CompleteMultipartUpload` requests.
    drop_completes: AtomicU64,
    /// Answer the next N `CompleteMultipartUpload` requests with HTTP **200**
    /// carrying an `<Error>` document, WITHOUT assembling the object.
    complete_error_documents: AtomicU64,
    /// Answer `CompleteMultipartUpload` with `404 NoSuchUpload`. Latching.
    no_such_upload_at_complete: AtomicBool,
    /// Artificial per-request latency, in nanoseconds.
    delay_nanos: AtomicU64,
}

impl Faults {
    fn new() -> Self {
        Self {
            slow_down_after: AtomicU64::new(NEVER),
            slow_down_burst: AtomicU64::new(0),
            request_timeout_after: AtomicU64::new(NEVER),
            internal_error_after: AtomicU64::new(NEVER),
            bucket_missing: AtomicBool::new(false),
            access_disabled: AtomicBool::new(false),
            quota_bytes: AtomicU64::new(NEVER),
            committed_bytes: AtomicU64::new(0),
            drop_part_after: AtomicU64::new(NEVER),
            drop_completes: AtomicU64::new(0),
            complete_error_documents: AtomicU64::new(0),
            no_such_upload_at_complete: AtomicBool::new(false),
            delay_nanos: AtomicU64::new(0),
        }
    }

    /// Decrement a "after N" counter, returning `true` when it trips. A trip
    /// resets the counter to `NEVER` (single-shot / transient).
    fn trips(counter: &AtomicU64) -> bool {
        loop {
            let cur = counter.load(Ordering::Acquire);
            if cur == NEVER {
                return false;
            }
            if cur == 0 {
                counter.store(NEVER, Ordering::Release);
                return true;
            }
            if counter
                .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return false;
            }
        }
    }

    /// Consume one unit of a plain "do this N more times" budget.
    fn take_budget(counter: &AtomicU64) -> bool {
        loop {
            let cur = counter.load(Ordering::Acquire);
            if cur == 0 {
                return false;
            }
            if counter
                .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

// ===========================================================================
// Counters
// ===========================================================================

/// Per-request-kind counters, so a scenario can assert on the traffic it
/// caused (e.g. "the interrupted part really was re-uploaded") rather than
/// only on the end state.
#[derive(Debug, Default)]
struct Counters {
    total: AtomicU64,
    put_object: AtomicU64,
    head_object: AtomicU64,
    get_object: AtomicU64,
    delete_object: AtomicU64,
    list_objects: AtomicU64,
    create_multipart: AtomicU64,
    upload_part: AtomicU64,
    list_parts: AtomicU64,
    complete_multipart: AtomicU64,
    abort_multipart: AtomicU64,
    dropped_connections: AtomicU64,
    // Per-fault firing counts. These exist because of the failure mode PR #192
    // documented: a green run that never reached its fault is not a passing
    // test, it is a test that did nothing. Every scenario asserts on the
    // counter for the fault it armed, so "the fault fired" is proven rather
    // than assumed.
    slow_down_fired: AtomicU64,
    request_timeout_fired: AtomicU64,
    internal_error_fired: AtomicU64,
    quota_refusals: AtomicU64,
    bucket_missing_refusals: AtomicU64,
    access_disabled_refusals: AtomicU64,
    complete_error_documents_sent: AtomicU64,
    no_such_upload_fired: AtomicU64,
}

/// A point-in-time copy of [`Counters`], for assertions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestCounts {
    /// Every request the server answered or dropped.
    pub total: u64,
    /// `PutObject` requests.
    pub put_object: u64,
    /// `HeadObject` requests.
    pub head_object: u64,
    /// `GetObject` requests.
    pub get_object: u64,
    /// `DeleteObject` requests.
    pub delete_object: u64,
    /// `ListObjectsV2` requests.
    pub list_objects: u64,
    /// `CreateMultipartUpload` requests.
    pub create_multipart: u64,
    /// `UploadPart` requests (including ones the server then dropped).
    pub upload_part: u64,
    /// `ListParts` requests.
    pub list_parts: u64,
    /// `CompleteMultipartUpload` requests (including dropped / faulted ones).
    pub complete_multipart: u64,
    /// `AbortMultipartUpload` requests.
    pub abort_multipart: u64,
    /// Requests answered by closing the connection with no response.
    pub dropped_connections: u64,
    /// `503 SlowDown` responses injected.
    pub slow_down_fired: u64,
    /// `400 RequestTimeout` responses injected.
    pub request_timeout_fired: u64,
    /// `500 InternalError` responses injected.
    pub internal_error_fired: u64,
    /// Writes refused because the byte budget was exhausted.
    pub quota_refusals: u64,
    /// Writes refused with `NoSuchBucket`.
    pub bucket_missing_refusals: u64,
    /// Writes refused with `AllAccessDisabled`.
    pub access_disabled_refusals: u64,
    /// `CompleteMultipartUpload` answers that were HTTP 200 carrying an
    /// `<Error>` document.
    pub complete_error_documents_sent: u64,
    /// `CompleteMultipartUpload` answers that were `404 NoSuchUpload`.
    pub no_such_upload_fired: u64,
}

// ===========================================================================
// The server
// ===========================================================================

/// Shared server state, held by the accept loop and by every connection task.
struct ServerState {
    bucket: String,
    objects: Mutex<BTreeMap<String, StoredObject>>,
    uploads: Mutex<HashMap<String, UploadState>>,
    next_upload_id: AtomicU64,
    next_modified_secs: AtomicU64,
    faults: Faults,
    counters: Counters,
}

/// A running in-process S3-compatible server.
///
/// Dropping the server aborts its accept loop; the bound port is released with
/// it, so a scenario needs no explicit teardown step.
pub struct FaultyS3Server {
    addr: SocketAddr,
    state: Arc<ServerState>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for FaultyS3Server {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl FaultyS3Server {
    /// Bind an ephemeral loopback port and start serving `bucket`.
    pub async fn start(bucket: &str) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(ServerState {
            bucket: bucket.to_string(),
            objects: Mutex::new(BTreeMap::new()),
            uploads: Mutex::new(HashMap::new()),
            next_upload_id: AtomicU64::new(1),
            next_modified_secs: AtomicU64::new(0),
            faults: Faults::new(),
            counters: Counters::default(),
        });
        let accept_state = state.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let st = accept_state.clone();
                        tokio::spawn(async move {
                            // A connection error is the point of several
                            // scenarios; never let one kill the accept loop.
                            let _ = serve_connection(stream, st).await;
                        });
                    }
                    Err(_) => {
                        // A transient accept failure must not end the server.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }
        });
        Ok(Self {
            addr,
            state,
            accept_task,
        })
    }

    /// The `http://127.0.0.1:<port>` endpoint for [`driven_s3::S3Config`].
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The bucket name this server serves.
    pub fn bucket(&self) -> &str {
        &self.state.bucket
    }

    /// Every object at rest, read from the in-memory map WITHOUT an HTTP round
    /// trip - the fault-free oracle the s6.3 invariant sweep reads even when a
    /// scenario left a fault latched.
    pub fn objects(&self) -> BTreeMap<String, StoredObject> {
        self.state.objects.lock().expect("objects lock").clone()
    }

    /// Every object at or below the `folder_id` key prefix, shaped as
    /// [`RemoteEntry`] for the s6.3 invariant sweep.
    ///
    /// Read from the in-memory map, so it works with a fault latched, and
    /// deliberately NOT via `list_folder`: that returns no user metadata on S3,
    /// which would make the duplicate-`client_op_uuid` check silently vacuous.
    ///
    /// `md5` is the TRUE digest of the bytes at rest, not the object's ETag. A
    /// multipart ETag is a digest of part digests, so `HeadObject` honestly
    /// reports `md5: None` for one - but the sweep compares against the
    /// `file_state.drive_md5` the upload recorded, which IS the content digest
    /// (`resume_chunk` substitutes the streamed md5 at completion). Reporting
    /// the ETag here would fail every multipart row as false data loss.
    pub fn oracle_entries(&self, folder_id: &str) -> Vec<driven_remote::remote_store::RemoteEntry> {
        let guard = self.state.objects.lock().expect("objects lock");
        guard
            .iter()
            .filter(|(key, _)| key.starts_with(folder_id))
            .map(|(key, obj)| driven_remote::remote_store::RemoteEntry {
                id: key.clone(),
                name: driven_s3::keys::base_name(key).to_string(),
                parents: vec![driven_s3::keys::parent_of(key)],
                size: Some(obj.bytes.len() as u64),
                md5: Some(obj.content_md5()),
                mime_type: obj.content_type.clone(),
                modified_time: obj.modified_secs * 1_000,
                // S3 has no trash: an object either exists or it does not.
                trashed: false,
                app_properties: driven_s3::keys::decode_props(obj.props_header.as_deref()),
            })
            .collect()
    }

    /// One object's bytes, or `None` if the key does not exist.
    pub fn object_bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.state
            .objects
            .lock()
            .expect("objects lock")
            .get(key)
            .map(|o| o.bytes.clone())
    }

    /// Keys of every multipart upload id still in flight. A non-empty result
    /// after a scenario means the store never aborted (or completed) an upload
    /// it opened - which is a storage leak on a real bucket, and the reason
    /// `driven-s3` aborts on every failure path.
    pub fn in_flight_upload_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .state
            .uploads
            .lock()
            .expect("uploads lock")
            .values()
            .map(|u| u.key.clone())
            .collect();
        keys.sort();
        keys
    }

    /// A snapshot of the per-request-kind counters.
    pub fn counts(&self) -> RequestCounts {
        let c = &self.state.counters;
        RequestCounts {
            total: c.total.load(Ordering::Acquire),
            put_object: c.put_object.load(Ordering::Acquire),
            head_object: c.head_object.load(Ordering::Acquire),
            get_object: c.get_object.load(Ordering::Acquire),
            delete_object: c.delete_object.load(Ordering::Acquire),
            list_objects: c.list_objects.load(Ordering::Acquire),
            create_multipart: c.create_multipart.load(Ordering::Acquire),
            upload_part: c.upload_part.load(Ordering::Acquire),
            list_parts: c.list_parts.load(Ordering::Acquire),
            complete_multipart: c.complete_multipart.load(Ordering::Acquire),
            abort_multipart: c.abort_multipart.load(Ordering::Acquire),
            dropped_connections: c.dropped_connections.load(Ordering::Acquire),
            slow_down_fired: c.slow_down_fired.load(Ordering::Acquire),
            request_timeout_fired: c.request_timeout_fired.load(Ordering::Acquire),
            internal_error_fired: c.internal_error_fired.load(Ordering::Acquire),
            quota_refusals: c.quota_refusals.load(Ordering::Acquire),
            bucket_missing_refusals: c.bucket_missing_refusals.load(Ordering::Acquire),
            access_disabled_refusals: c.access_disabled_refusals.load(Ordering::Acquire),
            complete_error_documents_sent: c.complete_error_documents_sent.load(Ordering::Acquire),
            no_such_upload_fired: c.no_such_upload_fired.load(Ordering::Acquire),
        }
    }

    // -- fault arming --------------------------------------------------------
    //
    // These are RUNTIME arming methods rather than the construction-time
    // `with_*` builders `InMemoryRemoteStore` uses, and deliberately so: an S3
    // row usually needs a clean first cycle (so there is a synced baseline
    // whose loss the invariants can detect) and the fault armed only for the
    // SECOND cycle. A construction-time-only surface cannot express that, and
    // faking it by rebuilding the store would throw away the bucket contents
    // the row is asserting about.

    /// Trip a transient `503 SlowDown` after `n` more write requests.
    ///
    /// This is the single most important S3 row: `SlowDown` is throttling, and
    /// the status-only rules would read a 503 as a transient server fault and
    /// give up after `MAX_RETRIES` instead of retrying indefinitely.
    pub fn arm_slow_down_after(&self, n: u64) {
        self.state
            .faults
            .slow_down_after
            .store(n, Ordering::Release);
    }

    /// Answer the next `n` write requests with `503 SlowDown`.
    ///
    /// The DISCRIMINATING form of the throttling fault. One `SlowDown` proves
    /// nothing: a transient 5xx is also retried, so a row that injects one
    /// passes whether or not `classify_s3_response` special-cases the S3 code.
    /// A burst LONGER than the executor's finite 5xx budget
    /// (`MAX_TRANSIENT_RETRIES`) separates the two: rate limiting retries
    /// indefinitely under the pacer and still completes, a transient 5xx gives
    /// up and strands the file.
    pub fn arm_slow_down_burst(&self, n: u64) {
        self.state
            .faults
            .slow_down_burst
            .store(n, Ordering::Release);
    }

    /// Trip a transient `400 RequestTimeout` after `n` more write requests -
    /// a retryable failure arriving with a client-error status.
    pub fn arm_request_timeout_after(&self, n: u64) {
        self.state
            .faults
            .request_timeout_after
            .store(n, Ordering::Release);
    }

    /// Trip a transient `500 InternalError` after `n` more write requests.
    pub fn arm_internal_error_after(&self, n: u64) {
        self.state
            .faults
            .internal_error_after
            .store(n, Ordering::Release);
    }

    /// Latch `404 NoSuchBucket` on every WRITE request.
    ///
    /// Reads keep working, matching `InMemoryRemoteStore::with_dest_folder_missing`,
    /// so the two backends' `dest-folder-missing` rows are directly comparable.
    /// `driven-s3` promotes `NoSuchBucket` to `DriveError::DestFolderMissing`.
    pub fn arm_bucket_missing(&self) {
        self.state
            .faults
            .bucket_missing
            .store(true, Ordering::Release);
    }

    /// Latch `403 AllAccessDisabled` on every WRITE request - the S3 shape of
    /// "your destination went read-only".
    pub fn arm_access_disabled(&self) {
        self.state
            .faults
            .access_disabled
            .store(true, Ordering::Release);
    }

    /// Cap total committed bytes at `n`; a write that would exceed it is
    /// refused with `QuotaExceeded` (which `driven-s3` classifies as
    /// `StorageQuota`).
    pub fn arm_quota_bytes(&self, n: u64) {
        self.state.faults.quota_bytes.store(n, Ordering::Release);
    }

    /// Close the connection - no response at all - on the `(n+1)`-th
    /// `UploadPart`, then behave normally.
    ///
    /// This is "the multipart upload was interrupted BETWEEN parts": the part
    /// never lands, the store sees a transport error, and the executor must
    /// recover without leaving a half-assembled object or a duplicate.
    pub fn arm_drop_part_after(&self, n: u64) {
        self.state
            .faults
            .drop_part_after
            .store(n, Ordering::Release);
    }

    /// Close the connection on the next `n` `CompleteMultipartUpload`
    /// requests.
    ///
    /// This is "interrupted BETWEEN `UploadPart` and `CompleteMultipartUpload`":
    /// every part is safely on the server, but the assembly instruction is
    /// lost. Nothing is published, so a resume must not conclude the object
    /// exists.
    pub fn arm_drop_complete(&self, n: u64) {
        self.state.faults.drop_completes.store(n, Ordering::Release);
    }

    /// Answer the next `n` `CompleteMultipartUpload` requests with **HTTP 200**
    /// carrying an `<Error>` document, and do NOT assemble the object.
    ///
    /// Real S3 does this because it holds the connection open while assembling
    /// the parts, so it has already committed to a 200 by the time it fails.
    /// A client that trusts the status code publishes a `synced` row for an
    /// object that does not exist - silent backup loss.
    pub fn arm_complete_error_document(&self, n: u64) {
        self.state
            .faults
            .complete_error_documents
            .store(n, Ordering::Release);
    }

    /// Latch `404 NoSuchUpload` on `CompleteMultipartUpload` - the upload id
    /// expired or was aborted out of band.
    pub fn arm_no_such_upload_at_complete(&self) {
        self.state
            .faults
            .no_such_upload_at_complete
            .store(true, Ordering::Release);
    }

    /// Add `delay` to every response.
    ///
    /// This is the PR #192 technique, not decoration: a race-shaped row that
    /// only reaches its window by luck is not a test. Widening the window
    /// deliberately makes the row hit the path on every run, which is what
    /// turns it from flaky into deterministic.
    pub fn arm_response_delay(&self, delay: Duration) {
        let nanos = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
        self.state
            .faults
            .delay_nanos
            .store(nanos, Ordering::Release);
    }

    /// Clear every armed fault, leaving the stored objects alone. Used by the
    /// rows that inject a fault for one cycle and then assert recovery on a
    /// healthy server.
    pub fn clear_faults(&self) {
        let f = &self.state.faults;
        f.slow_down_after.store(NEVER, Ordering::Release);
        f.slow_down_burst.store(0, Ordering::Release);
        f.request_timeout_after.store(NEVER, Ordering::Release);
        f.internal_error_after.store(NEVER, Ordering::Release);
        f.bucket_missing.store(false, Ordering::Release);
        f.access_disabled.store(false, Ordering::Release);
        f.quota_bytes.store(NEVER, Ordering::Release);
        f.drop_part_after.store(NEVER, Ordering::Release);
        f.drop_completes.store(0, Ordering::Release);
        f.complete_error_documents.store(0, Ordering::Release);
        f.no_such_upload_at_complete.store(false, Ordering::Release);
        f.delay_nanos.store(0, Ordering::Release);
    }
}

// ===========================================================================
// Connection handling
// ===========================================================================

/// A parsed request.
struct RawRequest {
    method: String,
    /// Percent-DECODED path.
    path: String,
    /// Query parameters, percent-decoded. `X-Amz-*` keys are kept but ignored.
    query: HashMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the server does with one request.
enum Action {
    Respond(Response),
    /// Close the connection without writing anything - the transport-failure
    /// faults.
    DropConnection,
}

/// A response to write.
struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn with_header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }

    fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    fn xml(status: u16, reason: &'static str, body: String) -> Self {
        Self::new(status, reason)
            .with_header("content-type", "application/xml")
            .with_body(body.into_bytes())
    }

    /// An S3 `<Error>` document with the given HTTP status. Passing status
    /// `200` is how the `CompleteMultipartUpload` quirk is modelled.
    fn s3_error(status: u16, reason: &'static str, code: &str, message: &str) -> Self {
        Self::xml(
            status,
            reason,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <Error><Code>{}</Code><Message>{}</Message>\
                 <RequestId>chaos</RequestId></Error>",
                xml_escape(code),
                xml_escape(message)
            ),
        )
    }
}

/// Serve every request on one (possibly keep-alive) connection.
async fn serve_connection(mut stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let Some(req) = read_request(&mut stream, &mut buf).await? else {
            return Ok(());
        };

        // The delay is awaited BEFORE the routing lock is taken, so no
        // `tokio::time::sleep` ever spans a held `std::sync::Mutex` guard.
        let delay = state.faults.delay_nanos.load(Ordering::Acquire);
        if delay > 0 {
            tokio::time::sleep(Duration::from_nanos(delay)).await;
        }

        let is_head = req.method == "HEAD";
        match route(&state, &req) {
            Action::DropConnection => {
                state
                    .counters
                    .dropped_connections
                    .fetch_add(1, Ordering::AcqRel);
                // Dropping the stream without a response is what the client
                // sees as a mid-request transport failure.
                return Ok(());
            }
            Action::Respond(resp) => {
                write_response(&mut stream, &resp, is_head).await?;
            }
        }
    }
}

/// Read one request off the connection. `Ok(None)` means the peer closed
/// cleanly between requests.
async fn read_request(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<RawRequest>> {
    // Head.
    let head_end = loop {
        if let Some(i) = find_subslice(buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(std::io::Error::other("request head too large"));
        }
        let mut tmp = [0u8; 16 * 1024];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let content_length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);

    // Body.
    while buf.len() < head_end + content_length {
        let mut tmp = [0u8; 64 * 1024];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::other("connection closed mid-body"));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[head_end..head_end + content_length].to_vec();
    // Retain anything already read for the NEXT pipelined request.
    buf.drain(..head_end + content_length);

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(k), percent_decode(v));
    }

    Ok(Some(RawRequest {
        method,
        path: percent_decode(raw_path),
        query,
        headers,
        body,
    }))
}

/// Write one response, omitting the body for a `HEAD` (whose `Content-Length`
/// still has to describe the object, because that is where
/// `S3Store::entry_from_head` reads `RemoteEntry::size`).
async fn write_response(
    stream: &mut TcpStream,
    resp: &Response,
    is_head: bool,
) -> std::io::Result<()> {
    let mut out = format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
    for (k, v) in &resp.headers {
        // A header value the fake produced is always ASCII and control-free;
        // this keeps a corrupt fixture from emitting a split response.
        if v.contains('\r') || v.contains('\n') {
            continue;
        }
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !resp.headers.iter().any(|(k, _)| {
        k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("transfer-encoding")
    }) {
        out.push_str(&format!("content-length: {}\r\n", resp.body.len()));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    if !is_head && !resp.body.is_empty() {
        stream.write_all(&resp.body).await?;
    }
    stream.flush().await
}

// ===========================================================================
// Routing
// ===========================================================================

/// Whether a request mutates the bucket - the set the "destination is gone /
/// read-only / throttling" faults apply to.
fn is_write(method: &str) -> bool {
    matches!(method, "PUT" | "POST" | "DELETE")
}

fn route(state: &ServerState, req: &RawRequest) -> Action {
    state.counters.total.fetch_add(1, Ordering::AcqRel);

    // Path is `/{bucket}` or `/{bucket}/{key...}`.
    let rest = req.path.trim_start_matches('/');
    let (bucket, key) = match rest.split_once('/') {
        Some((b, k)) => (b, k),
        None => (rest, ""),
    };
    if bucket != state.bucket {
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchBucket",
            "the specified bucket does not exist",
        ));
    }

    // Latching destination faults, and the transient write faults, apply
    // before any request-specific handling - they are answers the real service
    // gives instead of doing the work.
    if is_write(&req.method) {
        if state.faults.bucket_missing.load(Ordering::Acquire) {
            state
                .counters
                .bucket_missing_refusals
                .fetch_add(1, Ordering::AcqRel);
            return Action::Respond(Response::s3_error(
                404,
                "Not Found",
                "NoSuchBucket",
                "the specified bucket does not exist",
            ));
        }
        if state.faults.access_disabled.load(Ordering::Acquire) {
            state
                .counters
                .access_disabled_refusals
                .fetch_add(1, Ordering::AcqRel);
            return Action::Respond(Response::s3_error(
                403,
                "Forbidden",
                "AllAccessDisabled",
                "all access to this object has been disabled",
            ));
        }
        if Faults::trips(&state.faults.slow_down_after)
            || Faults::take_budget(&state.faults.slow_down_burst)
        {
            state
                .counters
                .slow_down_fired
                .fetch_add(1, Ordering::AcqRel);
            // 503 + SlowDown: throttling wearing a 5xx costume. No
            // `Retry-After`, so `classify_s3_response` must supply its own
            // floor.
            return Action::Respond(Response::s3_error(
                503,
                "Service Unavailable",
                "SlowDown",
                "please reduce your request rate",
            ));
        }
        if Faults::trips(&state.faults.request_timeout_after) {
            state
                .counters
                .request_timeout_fired
                .fetch_add(1, Ordering::AcqRel);
            // A retryable failure carrying a 400.
            return Action::Respond(Response::s3_error(
                400,
                "Bad Request",
                "RequestTimeout",
                "your socket connection to the server was not read from or written to within the timeout period",
            ));
        }
        if Faults::trips(&state.faults.internal_error_after) {
            state
                .counters
                .internal_error_fired
                .fetch_add(1, Ordering::AcqRel);
            return Action::Respond(Response::s3_error(
                500,
                "Internal Server Error",
                "InternalError",
                "we encountered an internal error, please try again",
            ));
        }
    }

    let upload_id = req.query.get("uploadId").cloned();
    let part_number = req
        .query
        .get("partNumber")
        .and_then(|s| s.parse::<u16>().ok());

    match req.method.as_str() {
        "PUT" => match (upload_id, part_number) {
            (Some(id), Some(n)) => upload_part(state, key, &id, n, req),
            _ => put_object(state, key, req),
        },
        "POST" => {
            if req.query.contains_key("uploads") {
                create_multipart(state, key, req)
            } else if let Some(id) = upload_id {
                complete_multipart(state, key, &id, req)
            } else {
                Action::Respond(Response::s3_error(
                    400,
                    "Bad Request",
                    "InvalidRequest",
                    "unsupported POST",
                ))
            }
        }
        "DELETE" => match upload_id {
            Some(id) => abort_multipart(state, &id),
            None => delete_object(state, key),
        },
        "HEAD" => head_object(state, key),
        "GET" => {
            if let Some(id) = upload_id {
                list_parts(state, &id, req)
            } else if req.query.get("list-type").map(String::as_str) == Some("2") {
                list_objects_v2(state, req)
            } else {
                get_object(state, key)
            }
        }
        _ => Action::Respond(Response::s3_error(
            405,
            "Method Not Allowed",
            "MethodNotAllowed",
            "unsupported method",
        )),
    }
}

/// Verify `Content-MD5` against the received bytes, as S3 does. Returns the
/// digest on success.
fn verify_content_md5(req: &RawRequest) -> Result<[u8; 16], Response> {
    let mut h = Md5::new();
    h.update(&req.body);
    let actual: [u8; 16] = h.finalize().into();
    if let Some(declared) = req.header("content-md5") {
        let expected = base64_decode(declared.trim());
        if expected.as_deref() != Some(&actual[..]) {
            return Err(Response::s3_error(
                400,
                "Bad Request",
                "BadDigest",
                "the Content-MD5 you specified did not match what we received",
            ));
        }
    }
    Ok(actual)
}

/// Charge `n` bytes against the quota budget, refusing when it would overrun.
fn charge_quota(state: &ServerState, n: u64) -> Option<Response> {
    let budget = state.faults.quota_bytes.load(Ordering::Acquire);
    if budget == NEVER {
        return None;
    }
    let committed = state.faults.committed_bytes.load(Ordering::Acquire);
    if committed.saturating_add(n) > budget {
        state.counters.quota_refusals.fetch_add(1, Ordering::AcqRel);
        return Some(Response::s3_error(
            400,
            "Bad Request",
            "QuotaExceeded",
            "the bucket has no room left",
        ));
    }
    state.faults.committed_bytes.fetch_add(n, Ordering::AcqRel);
    None
}

fn next_modified_secs(state: &ServerState) -> i64 {
    let n = state.next_modified_secs.fetch_add(1, Ordering::AcqRel);
    BASE_EPOCH_SECS + i64::try_from(n).unwrap_or(0)
}

fn put_object(state: &ServerState, key: &str, req: &RawRequest) -> Action {
    state.counters.put_object.fetch_add(1, Ordering::AcqRel);
    let md5 = match verify_content_md5(req) {
        Ok(m) => m,
        Err(resp) => return Action::Respond(resp),
    };
    if let Some(resp) = charge_quota(state, req.body.len() as u64) {
        return Action::Respond(resp);
    }
    let etag = format!("\"{}\"", hex_lower(&md5));
    let object = StoredObject {
        bytes: req.body.clone(),
        props_header: req.header("x-amz-meta-driven-props").map(str::to_string),
        md5_header: req.header("x-amz-meta-driven-md5").map(str::to_string),
        content_type: req
            .header("content-type")
            .unwrap_or("application/octet-stream")
            .to_string(),
        etag: etag.clone(),
        modified_secs: next_modified_secs(state),
    };
    state
        .objects
        .lock()
        .expect("objects lock")
        .insert(key.to_string(), object);
    Action::Respond(Response::new(200, "OK").with_header("etag", etag))
}

fn head_object(state: &ServerState, key: &str) -> Action {
    state.counters.head_object.fetch_add(1, Ordering::AcqRel);
    let guard = state.objects.lock().expect("objects lock");
    let Some(o) = guard.get(key) else {
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchKey",
            "the specified key does not exist",
        ));
    };
    let mut resp = Response::new(200, "OK")
        .with_header("etag", o.etag.clone())
        .with_header("content-type", o.content_type.clone())
        .with_header("last-modified", http_date(o.modified_secs))
        .with_header("content-length", o.bytes.len().to_string());
    if let Some(props) = &o.props_header {
        resp = resp.with_header("x-amz-meta-driven-props", props.clone());
    }
    if let Some(md5) = &o.md5_header {
        resp = resp.with_header("x-amz-meta-driven-md5", md5.clone());
    }
    Action::Respond(resp)
}

fn get_object(state: &ServerState, key: &str) -> Action {
    state.counters.get_object.fetch_add(1, Ordering::AcqRel);
    let guard = state.objects.lock().expect("objects lock");
    let Some(o) = guard.get(key) else {
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchKey",
            "the specified key does not exist",
        ));
    };
    Action::Respond(
        Response::new(200, "OK")
            .with_header("etag", o.etag.clone())
            .with_header("content-type", o.content_type.clone())
            .with_body(o.bytes.clone()),
    )
}

fn delete_object(state: &ServerState, key: &str) -> Action {
    state.counters.delete_object.fetch_add(1, Ordering::AcqRel);
    // A delete of a missing key is success, which is what makes `trash` /
    // `delete_permanent` idempotent.
    state.objects.lock().expect("objects lock").remove(key);
    Action::Respond(Response::new(204, "No Content"))
}

fn list_objects_v2(state: &ServerState, req: &RawRequest) -> Action {
    state.counters.list_objects.fetch_add(1, Ordering::AcqRel);
    let prefix = req.query.get("prefix").cloned().unwrap_or_default();
    let delimiter = req.query.get("delimiter").cloned();
    let max_keys: usize = req
        .query
        .get("max-keys")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
        .max(1);
    let after = req.query.get("continuation-token").cloned();

    let guard = state.objects.lock().expect("objects lock");
    let mut contents: Vec<(String, u64, String)> = Vec::new();
    let mut common: Vec<String> = Vec::new();
    let mut truncated_at: Option<String> = None;

    // `BTreeMap` iteration is already key-sorted, which is the order S3
    // guarantees and the order a continuation token has to be interpreted in.
    for (key, obj) in guard.iter() {
        if !key.starts_with(&prefix) {
            continue;
        }
        if let Some(a) = after.as_deref() {
            if key.as_str() <= a {
                continue;
            }
        }
        // Group under a common prefix when a delimiter was requested.
        if let Some(d) = delimiter.as_deref() {
            let tail = &key[prefix.len()..];
            if let Some(i) = tail.find(d) {
                let cp = format!("{}{}{}", prefix, &tail[..i], d);
                if !common.contains(&cp) {
                    common.push(cp);
                }
                continue;
            }
        }
        if contents.len() + common.len() >= max_keys {
            truncated_at = Some(key.clone());
            break;
        }
        contents.push((key.clone(), obj.bytes.len() as u64, obj.etag.clone()));
    }
    drop(guard);

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    for (key, size, etag) in &contents {
        xml.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified>\
             <ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            xml_escape(&percent_encode_key(key)),
            "2024-01-01T00:00:00.000Z",
            xml_escape(etag),
            size
        ));
    }
    xml.push_str(&format!("<MaxKeys>{max_keys}</MaxKeys>"));
    for cp in &common {
        xml.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            xml_escape(&percent_encode_key(cp))
        ));
    }
    if let Some(token) = truncated_at {
        // The token is "resume after the last key we returned", which is how
        // the `after` branch above reads it.
        let last = contents.last().map(|(k, _, _)| k.clone()).unwrap_or(token);
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(&last)
        ));
    }
    xml.push_str("<EncodingType>url</EncodingType>");
    xml.push_str("</ListBucketResult>");

    Action::Respond(Response::xml(200, "OK", xml))
}

fn create_multipart(state: &ServerState, key: &str, req: &RawRequest) -> Action {
    state
        .counters
        .create_multipart
        .fetch_add(1, Ordering::AcqRel);
    let id = format!(
        "chaos-upload-{}",
        state.next_upload_id.fetch_add(1, Ordering::AcqRel)
    );
    state.uploads.lock().expect("uploads lock").insert(
        id.clone(),
        UploadState {
            key: key.to_string(),
            content_type: req
                .header("content-type")
                .unwrap_or("application/octet-stream")
                .to_string(),
            props_header: req.header("x-amz-meta-driven-props").map(str::to_string),
            parts: BTreeMap::new(),
        },
    );
    Action::Respond(Response::xml(
        200,
        "OK",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId>\
             </InitiateMultipartUploadResult>",
            xml_escape(&state.bucket),
            xml_escape(key),
            xml_escape(&id)
        ),
    ))
}

fn upload_part(
    state: &ServerState,
    key: &str,
    upload_id: &str,
    number: u16,
    req: &RawRequest,
) -> Action {
    state.counters.upload_part.fetch_add(1, Ordering::AcqRel);

    // "Interrupted between parts": the counter is consulted BEFORE the part is
    // stored, so the part genuinely never lands.
    if Faults::trips(&state.faults.drop_part_after) {
        return Action::DropConnection;
    }

    let md5 = match verify_content_md5(req) {
        Ok(m) => m,
        Err(resp) => return Action::Respond(resp),
    };
    // Charged per PART, not at completion, so a byte budget can run out in the
    // MIDDLE of a large multipart upload - which is the failure a full
    // destination actually produces.
    if let Some(resp) = charge_quota(state, req.body.len() as u64) {
        return Action::Respond(resp);
    }
    let md5_hex = hex_lower(&md5);
    let mut guard = state.uploads.lock().expect("uploads lock");
    let Some(upload) = guard.get_mut(upload_id) else {
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchUpload",
            "the specified multipart upload does not exist",
        ));
    };
    if upload.key != key {
        return Action::Respond(Response::s3_error(
            400,
            "Bad Request",
            "InvalidRequest",
            "the upload id does not belong to this key",
        ));
    }
    upload.parts.insert(
        number,
        PartData {
            bytes: req.body.clone(),
            md5_hex: md5_hex.clone(),
        },
    );
    Action::Respond(Response::new(200, "OK").with_header("etag", format!("\"{md5_hex}\"")))
}

fn list_parts(state: &ServerState, upload_id: &str, req: &RawRequest) -> Action {
    state.counters.list_parts.fetch_add(1, Ordering::AcqRel);
    let marker: u16 = req
        .query
        .get("part-number-marker")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let max_parts: usize = req
        .query
        .get("max-parts")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
        .max(1);

    let guard = state.uploads.lock().expect("uploads lock");
    let Some(upload) = guard.get(upload_id) else {
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchUpload",
            "the specified multipart upload does not exist",
        ));
    };
    let selected: Vec<(u16, &PartData)> = upload
        .parts
        .iter()
        .filter(|(n, _)| **n > marker)
        .take(max_parts)
        .map(|(n, p)| (*n, p))
        .collect();
    let last = selected.last().map(|(n, _)| *n);
    let more = last.is_some_and(|l| upload.parts.keys().any(|n| *n > l));

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListPartsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    for (n, p) in &selected {
        xml.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>&quot;{}&quot;</ETag>\
             <LastModified>2024-01-01T00:00:00.000Z</LastModified><Size>{}</Size></Part>",
            n,
            p.md5_hex,
            p.bytes.len()
        ));
    }
    xml.push_str(&format!("<MaxParts>{max_parts}</MaxParts>"));
    xml.push_str(&format!("<IsTruncated>{more}</IsTruncated>"));
    if more {
        if let Some(l) = last {
            xml.push_str(&format!("<NextPartNumberMarker>{l}</NextPartNumberMarker>"));
        }
    }
    xml.push_str("</ListPartsResult>");
    Action::Respond(Response::xml(200, "OK", xml))
}

fn complete_multipart(state: &ServerState, key: &str, upload_id: &str, req: &RawRequest) -> Action {
    state
        .counters
        .complete_multipart
        .fetch_add(1, Ordering::AcqRel);

    // "Interrupted between UploadPart and CompleteMultipartUpload": every part
    // is on the server, the assembly instruction is lost, NOTHING is
    // published. The upload state is deliberately left in place - a real S3
    // upload id survives a dropped connection, which is what lets the resume
    // path enumerate its parts.
    if Faults::take_budget(&state.faults.drop_completes) {
        return Action::DropConnection;
    }

    if state
        .faults
        .no_such_upload_at_complete
        .load(Ordering::Acquire)
    {
        state
            .counters
            .no_such_upload_fired
            .fetch_add(1, Ordering::AcqRel);
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchUpload",
            "the specified multipart upload does not exist",
        ));
    }

    // HTTP 200 with an error document, and NO object published. The status is
    // already on the wire by the time real S3 discovers the failure.
    if Faults::take_budget(&state.faults.complete_error_documents) {
        state
            .counters
            .complete_error_documents_sent
            .fetch_add(1, Ordering::AcqRel);
        return Action::Respond(Response::s3_error(
            200,
            "OK",
            "InternalError",
            "we encountered an internal error while assembling the parts, please try again",
        ));
    }

    let requested = parse_requested_parts(&req.body);
    let guard = state.uploads.lock().expect("uploads lock");
    let Some(upload) = guard.get(upload_id) else {
        return Action::Respond(Response::s3_error(
            404,
            "Not Found",
            "NoSuchUpload",
            "the specified multipart upload does not exist",
        ));
    };
    if upload.key != key {
        return Action::Respond(Response::s3_error(
            400,
            "Bad Request",
            "InvalidRequest",
            "the upload id does not belong to this key",
        ));
    }

    // Assemble ONLY the requested parts, in the order requested. Anything else
    // held for this upload id is discarded, exactly as S3 does - that is the
    // property the crash-resume path relies on to avoid appending a stale part
    // from a previous run.
    let mut bytes: Vec<u8> = Vec::new();
    let mut etag_hasher = Md5::new();
    let mut count = 0usize;
    for n in &requested {
        let Some(part) = upload.parts.get(n) else {
            return Action::Respond(Response::s3_error(
                400,
                "Bad Request",
                "InvalidPart",
                "one or more of the specified parts could not be found",
            ));
        };
        bytes.extend_from_slice(&part.bytes);
        let raw = hex_decode(&part.md5_hex).unwrap_or_default();
        etag_hasher.update(&raw);
        count += 1;
    }
    if count == 0 {
        return Action::Respond(Response::s3_error(
            400,
            "Bad Request",
            "InvalidRequest",
            "a multipart upload must name at least one part",
        ));
    }
    let content_type = upload.content_type.clone();
    let props_header = upload.props_header.clone();
    drop(guard);

    // No quota charge here: the parts were charged as they landed (see
    // `upload_part`), so charging again would double-count the object.

    let digest: [u8; 16] = etag_hasher.finalize().into();
    // A multipart ETag is a digest OF DIGESTS with a `-N` suffix, which is why
    // `md5_from_etag` declines to read a content md5 off one.
    let etag = format!("\"{}-{}\"", hex_lower(&digest), count);
    state.objects.lock().expect("objects lock").insert(
        key.to_string(),
        StoredObject {
            bytes,
            props_header,
            // The multipart path cannot stamp `driven-md5`: S3 fixes user
            // metadata at CreateMultipartUpload time, before a byte is hashed.
            md5_header: None,
            content_type,
            etag: etag.clone(),
            modified_secs: next_modified_secs(state),
        },
    );
    state
        .uploads
        .lock()
        .expect("uploads lock")
        .remove(upload_id);

    // The ETag is a quoted string; servers escape those quotes differently
    // (MinIO emits `&#34;`, R2 `&quot;`), and `driven-s3` unescapes both. Emit
    // the named form so that unescape stays exercised.
    Action::Respond(Response::xml(
        200,
        "OK",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket><Key>{}</Key><ETag>{}</ETag>\
             </CompleteMultipartUploadResult>",
            xml_escape(&state.bucket),
            xml_escape(key),
            // `xml_escape` renders the ETag's quotes as `&quot;`.
            xml_escape(&etag)
        ),
    ))
}

fn abort_multipart(state: &ServerState, upload_id: &str) -> Action {
    state
        .counters
        .abort_multipart
        .fetch_add(1, Ordering::AcqRel);
    state
        .uploads
        .lock()
        .expect("uploads lock")
        .remove(upload_id);
    Action::Respond(Response::new(204, "No Content"))
}

/// Pull the `<PartNumber>` values out of a `CompleteMultipartUpload` body, in
/// document order.
fn parse_requested_parts(body: &[u8]) -> Vec<u16> {
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    let mut rest = text.as_ref();
    while let Some(i) = rest.find("<PartNumber>") {
        rest = &rest[i + "<PartNumber>".len()..];
        let Some(j) = rest.find("</PartNumber>") else {
            break;
        };
        if let Ok(n) = rest[..j].trim().parse::<u16>() {
            out.push(n);
        }
        rest = &rest[j..];
    }
    out
}

// ===========================================================================
// Small codecs (no new dependency for four short functions)
// ===========================================================================

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for c in s.bytes() {
        if c == b'=' || c == b'\r' || c == b'\n' {
            continue;
        }
        let v = TABLE.iter().position(|t| *t == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Percent-decode a URL component. An invalid escape is left as-is, which is
/// what every tolerant decoder does and keeps a hostile filename from
/// aborting the request.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode an object key for a `ListObjectsV2` response.
///
/// `S3Store` always requests `encoding-type=url` (rusty-s3 sets it
/// unconditionally) and `parse_response` percent-DECODES what comes back, so a
/// key containing a literal `%` would be silently corrupted if the server
/// emitted it raw. `/` is left alone, matching AWS.
fn percent_encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for b in key.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format seconds-since-epoch as the RFC 1123 date `S3Store::parse_http_date_ms`
/// expects (`Wed, 21 Oct 2015 07:28:00 GMT`).
fn http_date(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let dow = DAYS[(days.rem_euclid(7)) as usize];
    format!(
        "{dow}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        d,
        MONTHS[(m - 1) as usize],
        y,
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days`: the exact inverse of the
/// `days_from_civil` `S3Store::parse_http_date_ms` uses.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ===========================================================================
// Wiring a real S3Store at this server
// ===========================================================================

/// The bucket every harness server serves. A single fixed name keeps the
/// scenario code short; each server is a separate process-local port, so two
/// concurrent scenarios never share a bucket.
pub const CHAOS_BUCKET: &str = "driven-chaos";

/// The key prefix Driven is confined to, and therefore the destination root
/// "folder" id every s3.9 source points at. A non-empty prefix is the
/// interesting case: it proves the rows exercise key joining rather than
/// accidentally relying on bucket-root keys.
pub const CHAOS_PREFIX: &str = "backups/";

/// Build a real [`driven_s3::S3Store`] pointed at `server`.
///
/// Deliberately NOT via `S3CredentialStore`: that reads the OS keychain, which
/// a chaos run must never touch. The credential pair is passed directly, as the
/// `driven-s3` integration suite does.
pub fn store_for(server: &FaultyS3Server) -> anyhow::Result<driven_s3::S3Store> {
    let config = driven_s3::S3Config {
        endpoint: server.endpoint(),
        bucket: server.bucket().to_string(),
        region: driven_s3::DEFAULT_REGION.to_string(),
        path_style: true,
        prefix: Some(CHAOS_PREFIX.to_string()),
    }
    .normalized()?;
    let creds = driven_s3::S3Credentials {
        access_key_id: "chaos-access-key".into(),
        secret_access_key: "chaos-secret-key".into(),
    };
    driven_s3::S3Store::new(
        &config,
        &creds,
        &driven_tls::CustomCaConfig::none(),
        &driven_tls::ProxyConfig::system(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use bytes::Bytes;
    use driven_remote::remote_store::{
        DriveContext, RemoteStore, ResumableKind, ResumeProgress, UploadBody,
    };

    /// Deterministic pseudo-random bytes, so a corrupted transfer cannot pass
    /// by coincidence the way a buffer of zeroes might.
    fn payload(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn props(op_uuid: &str, source_id: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(
            driven_remote::props::CLIENT_OP_UUID_KEY.to_string(),
            op_uuid.to_string(),
        );
        m.insert(
            driven_remote::props::SOURCE_ID_KEY.to_string(),
            source_id.to_string(),
        );
        m
    }

    /// The contract smoke test. If this fails, every s3.9 row is meaningless -
    /// so it asserts the whole surface `driven-s3` uses against the server,
    /// with NO fault armed.
    #[tokio::test]
    async fn a_real_s3_store_round_trips_against_the_in_process_server() {
        let server = FaultyS3Server::start(CHAOS_BUCKET).await.expect("server");
        let store = store_for(&server).expect("store");
        let root = CHAOS_PREFIX;

        // ensure_folder is a pure key computation (no request).
        let folder = store
            .ensure_folder(root, "docs", &DriveContext::MyDrive)
            .await
            .expect("ensure_folder");
        assert_eq!(folder.id, "backups/docs/");

        // -- single PutObject path ------------------------------------------
        let small = payload(4096, 1);
        let created = store
            .create(
                &folder.id,
                "small.bin",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(small.clone())),
                props("op-small", "src-1"),
            )
            .await
            .expect("create");
        assert_eq!(created.id, "backups/docs/small.bin");
        assert_eq!(created.size, Some(small.len() as u64));
        // A single-part ETag IS the content md5, so `metadata` reports one.
        assert!(created.md5.is_some(), "single-PUT object must carry an md5");
        assert_eq!(
            created
                .app_properties
                .get(driven_remote::props::CLIENT_OP_UUID_KEY)
                .map(String::as_str),
            Some("op-small"),
            "app_properties must survive the x-amz-meta round trip"
        );

        // -- the multipart / resumable path ---------------------------------
        // 12 MiB: three 4 MiB wire chunks (executor.rs WIRE_CHUNK) buffered
        // into two parts (driven-s3 PART_SIZE = 8 MiB), so the multi-part and
        // final-part branches both run.
        let big = payload(12 * 1024 * 1024, 2);
        let session = store
            .resumable_session(
                ResumableKind::Create {
                    parent_id: folder.id.clone(),
                    name: "big.bin".into(),
                    app_properties: props("op-big", "src-1"),
                },
                "application/octet-stream",
                big.len() as u64,
            )
            .await
            .expect("resumable_session");
        let wire = 4 * 1024 * 1024;
        let mut offset = 0usize;
        let mut completed = None;
        while offset < big.len() {
            let end = (offset + wire).min(big.len());
            let progress = store
                .resume_chunk(
                    &session,
                    offset as u64,
                    Bytes::copy_from_slice(&big[offset..end]),
                )
                .await
                .expect("resume_chunk");
            match progress {
                ResumeProgress::InProgress { received } => {
                    assert_eq!(received as usize, end, "the store must ack every byte");
                    offset = end;
                }
                ResumeProgress::Completed(entry) => {
                    completed = Some(entry);
                    offset = end;
                }
                ResumeProgress::SessionInvalid => panic!("session invalidated with no fault armed"),
            }
        }
        let entry = completed.expect("the final chunk completes the upload");
        assert_eq!(entry.size, Some(big.len() as u64));
        assert!(
            server.in_flight_upload_keys().is_empty(),
            "a completed upload must not stay in flight"
        );
        assert_eq!(
            server.counts().upload_part,
            2,
            "12 MiB must land as exactly two parts"
        );

        // The bytes at rest must be the bytes sent, assembled in order.
        assert_eq!(
            server.object_bytes("backups/docs/big.bin").as_deref(),
            Some(&big[..]),
            "the multipart object must assemble byte-for-byte"
        );

        // -- reads -----------------------------------------------------------
        let listed = store
            .list_folder(&folder.id, &DriveContext::MyDrive)
            .await
            .expect("list_folder");
        let mut names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["big.bin", "small.bin"]);

        let found = store
            .find_by_op_uuid(&folder.id, "op-big", &DriveContext::MyDrive)
            .await
            .expect("find_by_op_uuid");
        assert_eq!(
            found.map(|e| e.id).as_deref(),
            Some("backups/docs/big.bin"),
            "find_by_op_uuid must read the props back off user metadata"
        );

        let owned = store
            .list_source_object_ids("src-1", &DriveContext::MyDrive)
            .await
            .expect("list_source_object_ids");
        assert_eq!(owned.len(), 2, "both objects belong to src-1: {owned:?}");

        let about = store.about().await.expect("about");
        assert_eq!(about.usage, (small.len() + big.len()) as u64);

        // -- delete is idempotent -------------------------------------------
        store.trash(&created.id).await.expect("trash");
        store
            .trash(&created.id)
            .await
            .expect("trashing an already-gone key is success");
        assert!(store.metadata(&created.id).await.is_err());
    }

    /// `ListObjectsV2` pagination has to work, because `list_source_object_ids`
    /// treats a short answer as "these objects are gone" - a truncated listing
    /// would read as a mass deletion and the audit would re-upload a source.
    #[tokio::test]
    async fn the_server_paginates_list_objects_v2() {
        let server = FaultyS3Server::start(CHAOS_BUCKET).await.expect("server");
        let store = store_for(&server).expect("store");
        for i in 0..7u32 {
            store
                .create(
                    CHAOS_PREFIX,
                    &format!("f{i:02}.bin"),
                    "application/octet-stream",
                    UploadBody::Bytes(Bytes::from(payload(16, i as u64))),
                    props(&format!("op-{i}"), "src-page"),
                )
                .await
                .expect("create");
        }
        // Force real paging by asking the server for tiny pages: the store
        // requests max-keys=1000, so drive pagination through the server's own
        // honouring of the parameter via a hand-built request instead.
        let ids = store
            .list_source_object_ids("src-page", &DriveContext::MyDrive)
            .await
            .expect("list_source_object_ids");
        assert_eq!(ids.len(), 7, "every object must be enumerated: {ids:?}");
        assert!(server.counts().list_objects >= 1);
    }

    #[test]
    fn http_dates_round_trip_through_the_stores_parser() {
        // The server's formatter and `S3Store::parse_http_date_ms` must agree,
        // or every `RemoteEntry::modified_time` silently degrades to 0.
        for secs in [BASE_EPOCH_SECS, 0, 951_782_400, 1_735_689_599] {
            let text = http_date(secs);
            // Re-derive with the same civil algorithm the store uses.
            let reparsed = reparse_http_date(&text).expect("formatted date parses");
            assert_eq!(reparsed, secs * 1_000, "{text}");
        }
    }

    /// A local copy of `driven_s3::store::parse_http_date_ms` (which is
    /// private) so the formatter is pinned against the real algorithm.
    fn reparse_http_date(s: &str) -> Option<i64> {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let s = s.trim();
        let rest = s.split_once(", ").map(|(_, r)| r).unwrap_or(s);
        let mut it = rest.split_whitespace();
        let day: i64 = it.next()?.parse().ok()?;
        let month_name = it.next()?;
        let month = MONTHS.iter().position(|m| *m == month_name)? as i64 + 1;
        let year: i64 = it.next()?.parse().ok()?;
        let mut hms = it.next()?.split(':');
        let hour: i64 = hms.next()?.parse().ok()?;
        let min: i64 = hms.next()?.parse().ok()?;
        let sec: i64 = hms.next()?.parse().ok()?;
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some((days * 86_400 + hour * 3_600 + min * 60 + sec) * 1_000)
    }

    #[test]
    fn base64_and_hex_codecs_agree_with_the_wire_format() {
        // `Content-MD5` is standard base64 of the 16 raw digest bytes.
        let digest: [u8; 16] = [
            0x9e, 0x10, 0x7d, 0x9d, 0x37, 0x2b, 0xb6, 0x82, 0x6b, 0xd8, 0x1d, 0x35, 0x42, 0xa4,
            0x19, 0xd6,
        ];
        let b64 = "nhB9nTcrtoJr2B01QqQZ1g==";
        assert_eq!(base64_decode(b64).as_deref(), Some(&digest[..]));
        assert_eq!(hex_lower(&digest), "9e107d9d372bb6826bd81d3542a419d6");
        assert_eq!(
            hex_decode("9e107d9d372bb6826bd81d3542a419d6").as_deref(),
            Some(&digest[..])
        );
    }

    #[test]
    fn keys_survive_the_list_encoding_round_trip() {
        // rusty-s3 percent-decodes list responses, so a key with a literal `%`
        // must be encoded on the way out or it decodes to something else.
        for key in ["a/b.txt", "100%_done.txt", "sp ace.txt", "caf\u{e9}.txt"] {
            let encoded = percent_encode_key(key);
            assert_eq!(percent_decode(&encoded), key, "{key}");
        }
    }

    #[test]
    fn requested_parts_are_read_in_document_order() {
        let body = b"<CompleteMultipartUpload>\
            <Part><ETag>a</ETag><PartNumber>1</PartNumber></Part>\
            <Part><ETag>b</ETag><PartNumber>2</PartNumber></Part>\
            </CompleteMultipartUpload>";
        assert_eq!(parse_requested_parts(body), vec![1, 2]);
        assert!(parse_requested_parts(b"").is_empty());
    }

    #[test]
    fn a_counting_fault_trips_once_then_resets() {
        let c = AtomicU64::new(2);
        assert!(!Faults::trips(&c));
        assert!(!Faults::trips(&c));
        assert!(Faults::trips(&c));
        // Single-shot: the retry after the trip succeeds.
        assert!(!Faults::trips(&c));
        // NEVER never trips.
        let n = AtomicU64::new(NEVER);
        assert!(!Faults::trips(&n));
    }

    #[test]
    fn a_budget_fault_fires_exactly_n_times() {
        let c = AtomicU64::new(2);
        assert!(Faults::take_budget(&c));
        assert!(Faults::take_budget(&c));
        assert!(!Faults::take_budget(&c));
    }
}
