//! [`S3Store`] - the `RemoteStore` implementation for S3-compatible services.
//!
//! ## Integrity (read this before touching the upload paths)
//!
//! The executor verifies every upload by comparing the `RemoteEntry.md5` this
//! store returns against the md5 it computed locally over the exact bytes it
//! sent (`executor.rs`, "md5 verify over the exact bytes sent"). That check is
//! only meaningful if the md5 we return is SERVER-derived or server-VERIFIED. It
//! is, by two different mechanisms:
//!
//! - **Single `PutObject`** - the request carries `Content-MD5`, which S3
//!   verifies against the body before storing it (a mismatch is `BadDigest` and
//!   nothing is written), and the response `ETag` for a single-part object IS
//!   the content md5. We return the ETag, so the value is the server's.
//! - **Multipart** - the object's ETag is `md5(concat(part md5s))-N`, NOT the
//!   content md5, so there is no server-side content digest to return. Instead
//!   EVERY `UploadPart` carries its own `Content-MD5` (so the server verifies
//!   each part's bytes), and the `CompleteMultipartUpload` response ETag is
//!   checked against the composed digest we compute from those same part md5s
//!   (so the server confirms it assembled exactly the parts we sent, in order).
//!   Only then do we return the locally-computed full-content md5.
//!
//! **Removing either the per-part `Content-MD5` or the composed-ETag check
//! silently disables corruption detection for every file above
//! [`MULTIPART_THRESHOLD`]**, because the executor would then be comparing our
//! local md5 against itself. They are load-bearing, not belt-and-braces.
//!
//! [`Self::metadata`] is the one place a content md5 may be absent: see its doc.
//!
//! ## Folders, trash, and other S3-isms
//!
//! - Folders are key prefixes; see [`crate::keys`]. `ensure_folder` performs no
//!   request and writes no marker object.
//! - **S3 has no trash.** `trash` is a permanent `DeleteObject`, identical to
//!   `delete_permanent`. A user who wants Drive-like recovery enables bucket
//!   versioning, which is a server-side setting outside Driven's control. This
//!   is called out in the setup UI rather than silently pretended away.
//! - `list_shared_drives` keeps the trait's empty default: S3 has no equivalent
//!   corpus notion, and `DriveContext` is accepted and ignored throughout.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use driven_remote::remote_store::{
    AboutInfo, DownloadStream, DriveContext, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, UploadBody,
};
use driven_remote::{retry, DriveError};
use driven_tls::{CustomCaConfig, ProxyConfig};
use futures::StreamExt;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use rusty_s3::{actions, Bucket, Credentials, S3Action, UrlStyle};

use crate::config::{S3Config, S3Credentials};
use crate::error::{is_not_found, s3_error_from_response};
use crate::http::{build_meta_client, build_stream_client, parse_retry_after, BodyReader};
use crate::keys;

/// Multipart part size.
///
/// Three constraints pin this value:
/// 1. S3 requires every NON-FINAL part to be at least 5 MiB.
/// 2. Cloudflare R2 additionally requires every non-final part to be the SAME
///    size.
/// 3. `driven-core`'s executor pushes fixed 4 MiB wire chunks
///    (`executor.rs::WIRE_CHUNK`), which is BELOW S3's minimum - so this store
///    must buffer chunks into parts itself rather than the core widening its
///    chunk for an S3-specific reason.
///
/// 8 MiB satisfies (1), and being an exact multiple of the 4 MiB wire chunk
/// means whole chunks pack into a part with no partial-chunk bookkeeping, which
/// satisfies (2) for every part but the last.
pub const PART_SIZE: usize = 8 * 1024 * 1024;

/// At or above this size an upload goes multipart; below it is a single
/// `PutObject`.
pub const MULTIPART_THRESHOLD: u64 = PART_SIZE as u64;

// Compile-time proof of the two constraints above, so a future edit to
// PART_SIZE cannot quietly break every multipart upload (mirrors
// `executor.rs`'s WIRE_CHUNK assertion):
//   - S3 rejects a non-final part below 5 MiB outright;
//   - being an exact multiple of the executor's 4 MiB wire chunk is what keeps
//     every non-final part the same size, which Cloudflare R2 requires.
const CORE_WIRE_CHUNK: usize = 4 * 1024 * 1024;
const _: () = assert!(PART_SIZE >= 5 * 1024 * 1024);
const _: () = assert!(PART_SIZE % CORE_WIRE_CHUNK == 0);

/// Validity window for the presigned URLs this store mints. Every URL is used
/// immediately by the request that generated it, so this only has to absorb
/// clock skew and one retry cycle.
const SIGN_EXPIRY: Duration = Duration::from_secs(15 * 60);

/// Objects per `ListObjectsV2` page.
const LIST_PAGE_SIZE: usize = 1000;

/// Uploads per `ListMultipartUploads` page.
const UPLOAD_LIST_PAGE_SIZE: usize = 1000;

/// S3's ceiling on a single `CopyObject`. Above it a copy must go through
/// multipart (`UploadPartCopy`).
const MAX_SINGLE_COPY: u64 = 5 * 1024 * 1024 * 1024;

/// Byte range each `UploadPartCopy` moves when a copy is too big for one
/// `CopyObject`.
///
/// 1 GiB keeps every non-final part the same size (which Cloudflare R2
/// requires) while staying inside the 10,000-part limit for any object S3 can
/// hold: 5 TiB / 1 GiB = 5,120 parts.
const COPY_PART_SIZE: u64 = 1024 * 1024 * 1024;

/// How long an abandoned multipart upload may sit before
/// [`S3Store::sweep_abandoned_multipart_uploads`] aborts it (issue #222).
///
/// S3 and R2 bill for the parts of an incomplete multipart upload until it is
/// aborted, so a leak here is real money. The threshold is NOT tighter than
/// this on purpose: an upload id that outlived its process is exactly what the
/// executor's crash-resume path re-attaches to, and Driven keeps a persisted
/// resumable session for up to 6 days (`executor.rs::SESSION_MAX_AGE_MS`). At 7
/// days the sweep can never abort a session anything would still resume, which
/// is the same window - and the same reasoning - `driven-localfs` uses for its
/// abandoned temp files.
const ABANDONED_UPLOAD_MIN_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Concurrent `HeadObject` requests when reading `app_properties` for a whole
/// prefix (`list_source_object_ids`, `find_by_op_uuid`).
const HEAD_CONCURRENCY: usize = 16;

/// MIME type reported for a synthesized folder entry.
const FOLDER_MIME: &str = "application/x-directory";

/// Prefix marking a [`ResumableSession::url`] as this backend's encoded handle
/// rather than a real URL.
const SESSION_URL_SCHEME: &str = "driven-s3:";

/// One part of an in-flight multipart upload.
#[derive(Debug, Clone)]
struct PartRecord {
    number: u16,
    /// The part's own md5 - both the `Content-MD5` we sent and (for a
    /// single-part upload) the ETag the server returned.
    md5: [u8; 16],
    size: u64,
}

/// The store-side state of one in-flight multipart upload.
///
/// The key and upload id are NOT held here: they are recovered from the
/// caller-persisted [`ResumableSession::url`] on every call, which is the only
/// copy that survives a process restart.
struct MultipartState {
    /// Parts already on the server from a PREVIOUS process, hydrated from
    /// `ListParts` when a persisted session is resumed: part number -> (etag,
    /// size). A part whose bytes re-derive to the same md5 is skipped rather
    /// than re-uploaded.
    existing: HashMap<u16, (String, u64)>,
    /// Parts accounted for in THIS run, in ascending part-number order.
    parts: Vec<PartRecord>,
    /// Bytes buffered toward the next part.
    buffer: Vec<u8>,
    /// Object offset at which `buffer` starts.
    buffer_start: u64,
    /// Running md5 over every byte seen so far (flushed parts + buffer).
    md5: Md5,
    /// Whether this state was hydrated from a persisted session and has already
    /// asked the caller to rewind to offset 0. Bounded to once, so a caller
    /// that ignores the rewind cannot spin forever.
    rewound: bool,
}

impl MultipartState {
    fn new() -> Self {
        Self {
            existing: HashMap::new(),
            parts: Vec::new(),
            buffer: Vec::new(),
            buffer_start: 0,
            md5: Md5::new(),
            rewound: true, // a fresh session starts at 0 already
        }
    }

    /// Bytes of the object this state has consumed (flushed + buffered).
    fn consumed(&self) -> u64 {
        self.buffer_start + self.buffer.len() as u64
    }
}

/// The S3-compatible [`RemoteStore`].
pub struct S3Store {
    bucket: Bucket,
    credentials: Credentials,
    /// Slash-terminated key prefix, or `""` for the bucket root.
    prefix: String,
    /// Whether the endpoint is addressed path-style (`host/bucket/key`) rather
    /// than virtual-host-style (`bucket.host/key`). Needed to build the
    /// `x-amz-copy-source` header, which always names the bucket explicitly.
    path_style: bool,
    /// Total-capped client for metadata / control requests.
    http: reqwest::Client,
    /// Idle-timeout-only client for body transfers.
    http_stream: reqwest::Client,
    /// In-flight RESUMABLE uploads, keyed by upload id.
    uploads: Mutex<HashMap<String, MultipartState>>,
    /// EVERY multipart upload id this process has open (resumable sessions,
    /// streamed `multipart_stream` uploads, and multipart archive copies).
    ///
    /// The abandoned-upload sweep consults this so it can never abort an upload
    /// this very process is in the middle of - the age threshold alone would be
    /// enough in practice, but a store constructed while a long transfer runs
    /// (a second account, a re-probe) must not be able to shoot it. Issue #222.
    owned_uploads: Mutex<HashSet<String>>,
}

impl S3Store {
    /// Build a store for `config` authenticated with `creds`, applying the
    /// corporate custom-CA + proxy configuration to both clients (issue #34).
    pub fn new(
        config: &S3Config,
        creds: &S3Credentials,
        ca: &CustomCaConfig,
        proxy: &ProxyConfig,
    ) -> anyhow::Result<Self> {
        let endpoint = config
            .endpoint
            .parse::<url::Url>()
            .map_err(|e| anyhow::anyhow!("s3.config_invalid: endpoint is not a URL: {e}"))?;
        let style = if config.path_style {
            UrlStyle::Path
        } else {
            UrlStyle::VirtualHost
        };
        let bucket = Bucket::new(
            endpoint,
            style,
            config.bucket.clone(),
            config.region.clone(),
        )
        .map_err(|e| anyhow::anyhow!("s3.config_invalid: {e}"))?;
        Ok(Self {
            bucket,
            credentials: Credentials::new(
                creds.access_key_id.clone(),
                creds.secret_access_key.clone(),
            ),
            prefix: config.root_prefix().to_string(),
            path_style: config.path_style,
            http: build_meta_client(ca, proxy)?,
            http_stream: build_stream_client(ca, proxy)?,
            uploads: Mutex::new(HashMap::new()),
            owned_uploads: Mutex::new(HashSet::new()),
        })
    }

    /// The destination root "folder" id: the configured prefix.
    pub fn root_id(&self) -> &str {
        &self.prefix
    }

    // -- request plumbing ----------------------------------------------------

    /// Execute a signed request, turning any non-2xx response into a classified
    /// [`DriveError`]. Returns the successful response for the caller to read.
    async fn execute(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        url: url::Url,
        headers: &[(String, String)],
        body: Option<Bytes>,
    ) -> anyhow::Result<reqwest::Response> {
        let mut req = client.request(method, url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = body {
            req = req.body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(resp);
        }
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.bytes().await.unwrap_or_default();
        Err(anyhow::Error::new(s3_error_from_response(
            status,
            &body,
            retry_after,
        )))
    }

    /// Execute a request through the shared retry middleware. ONLY for
    /// idempotent requests: a blind retry of a non-idempotent one could double
    /// an effect.
    async fn execute_retrying(
        &self,
        client: &reqwest::Client,
        method: reqwest::Method,
        url: url::Url,
        headers: Vec<(String, String)>,
    ) -> anyhow::Result<reqwest::Response> {
        retry::with_retry(|| {
            let url = url.clone();
            let headers = headers.clone();
            let method = method.clone();
            async move { self.execute(client, method, url, &headers, None).await }
        })
        .await
    }

    // -- metadata mapping ----------------------------------------------------

    /// Build a [`RemoteEntry`] for an object from a `HeadObject` response.
    fn entry_from_head(&self, key: &str, resp: &reqwest::Response) -> RemoteEntry {
        let headers = resp.headers();
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let props = keys::decode_props(
            header(&format!("x-amz-meta-{}", keys::PROPS_METADATA_KEY)).as_deref(),
        );
        let etag = header("etag").unwrap_or_default();
        let md5 = header(&format!("x-amz-meta-{}", keys::CONTENT_MD5_METADATA_KEY))
            .and_then(|h| parse_md5_hex(&h))
            .or_else(|| md5_from_etag(&etag));
        RemoteEntry {
            id: key.to_string(),
            name: keys::base_name(key).to_string(),
            parents: vec![keys::parent_of(key)],
            size: header("content-length").and_then(|v| v.parse::<u64>().ok()),
            md5,
            mime_type: header("content-type").unwrap_or_else(|| "application/octet-stream".into()),
            modified_time: header("last-modified")
                .and_then(|v| parse_http_date_ms(&v))
                .unwrap_or(0),
            // S3 has no trash: an object either exists or it does not.
            trashed: false,
            app_properties: props,
        }
    }

    /// `HeadObject` for one key.
    async fn head(&self, key: &str) -> anyhow::Result<RemoteEntry> {
        let action = actions::HeadObject::new(&self.bucket, Some(&self.credentials), key);
        let url = action.sign(SIGN_EXPIRY);
        let resp = self
            .execute_retrying(&self.http, reqwest::Method::HEAD, url, Vec::new())
            .await?;
        Ok(self.entry_from_head(key, &resp))
    }

    /// Page through `ListObjectsV2` under `prefix`, invoking `on_page` for each
    /// page's contents and common prefixes.
    ///
    /// Returns `Err` on ANY page failure and never a partial result: callers
    /// (notably `list_source_object_ids`) treat a short answer as "these objects
    /// are gone", so a silently truncated listing would read as a mass deletion.
    async fn list_pages<F>(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
        mut on_page: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(actions::list_objects_v2::ListObjectsV2Response),
    {
        let mut token: Option<String> = None;
        loop {
            let mut action = actions::ListObjectsV2::new(&self.bucket, Some(&self.credentials));
            action.with_max_keys(LIST_PAGE_SIZE);
            if !prefix.is_empty() {
                action.with_prefix(prefix.to_string());
            }
            if let Some(d) = delimiter {
                action.with_delimiter(d.to_string());
            }
            if let Some(t) = token.as_deref() {
                action.with_continuation_token(t.to_string());
            }
            let url = action.sign(SIGN_EXPIRY);
            let resp = self
                .execute_retrying(&self.http, reqwest::Method::GET, url, Vec::new())
                .await?;
            let text = resp
                .text()
                .await
                .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
            let parsed = actions::ListObjectsV2::parse_response(&text).map_err(|e| {
                anyhow::anyhow!("s3.list_failed: could not parse a ListObjectsV2 response: {e}")
            })?;
            token = parsed.next_continuation_token.clone();
            on_page(parsed);
            if token.is_none() {
                return Ok(());
            }
        }
    }

    /// HEAD every key with bounded concurrency, returning `(key, entry)` pairs.
    /// Any failure aborts with `Err` - never a partial map.
    async fn head_all(&self, kys: Vec<String>) -> anyhow::Result<Vec<RemoteEntry>> {
        let results =
            futures::stream::iter(kys.into_iter().map(|k| async move { self.head(&k).await }))
                .buffer_unordered(HEAD_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        results.into_iter().collect()
    }

    // -- uploads -------------------------------------------------------------

    /// Headers carrying `app_properties` (and optionally the content md5) as S3
    /// user metadata.
    fn metadata_headers(
        props: &HashMap<String, String>,
        content_md5: Option<&[u8; 16]>,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if let Some(encoded) = keys::encode_props(props)? {
            out.push((format!("x-amz-meta-{}", keys::PROPS_METADATA_KEY), encoded));
        }
        if let Some(md5) = content_md5 {
            out.push((
                format!("x-amz-meta-{}", keys::CONTENT_MD5_METADATA_KEY),
                hex::encode(md5),
            ));
        }
        Ok(out)
    }

    /// Single-request upload with server-side digest verification.
    async fn put_object(
        &self,
        key: &str,
        mime: &str,
        body: Bytes,
        props: &HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let md5 = md5_of(&body);
        let mut headers = Self::metadata_headers(props, Some(&md5))?;
        headers.push(("content-type".to_string(), mime.to_string()));
        // S3 verifies this against the body it receives and refuses to store a
        // mismatch (`BadDigest`), which is what makes the ETag we read back a
        // trustworthy witness rather than an echo of our own bytes.
        headers.push((
            "content-md5".to_string(),
            base64::engine::general_purpose::STANDARD.encode(md5),
        ));

        let mut action = actions::PutObject::new(&self.bucket, Some(&self.credentials), key);
        for (k, v) in &headers {
            action.headers_mut().insert(k.clone(), v.clone());
        }
        let url = action.sign(SIGN_EXPIRY);

        // NOT retried: a PUT is idempotent by key, but the body is a one-shot
        // `Bytes` and a blind retry here would hide a real failure from the
        // pacer. The executor's own retry ladder covers the operation.
        let resp = self
            .execute(
                &self.http_stream,
                reqwest::Method::PUT,
                url,
                &headers,
                Some(body),
            )
            .await?;

        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_default();
        match md5_from_etag(&etag) {
            Some(server) if server == md5 => {}
            _ => {
                // The server accepted the Content-MD5 but reported a different
                // ETag: something between us and the bytes at rest is wrong.
                // Delete the object so the failure leaves nothing behind, then
                // report the mismatch the executor knows how to handle.
                let stranded = match self.delete_key(key).await {
                    Ok(()) => None,
                    Err(_) => Some(key.to_string()),
                };
                return Err(anyhow::Error::new(DriveError::ChecksumMismatch {
                    stranded_file_id: stranded,
                }));
            }
        }

        self.head(key).await
    }

    /// Stream an object in through a multipart upload, with per-part digest
    /// verification and a composed-ETag check at completion.
    async fn multipart_stream(
        &self,
        key: &str,
        mime: &str,
        len: u64,
        props: &HashMap<String, String>,
        mut stream: Box<dyn futures::Stream<Item = anyhow::Result<Bytes>> + Send + Unpin>,
    ) -> anyhow::Result<RemoteEntry> {
        let upload_id = self.create_multipart(key, mime, props).await?;
        let mut parts: Vec<PartRecord> = Vec::new();
        let mut buffer: Vec<u8> = Vec::with_capacity(PART_SIZE);
        let mut hasher = Md5::new();
        let mut seen: u64 = 0;
        let mut number: u16 = 1;

        let result = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                seen += chunk.len() as u64;
                hasher.update(&chunk);
                buffer.extend_from_slice(&chunk);
                while buffer.len() >= PART_SIZE {
                    let rest = buffer.split_off(PART_SIZE);
                    let part = std::mem::replace(&mut buffer, rest);
                    parts.push(self.upload_part(key, &upload_id, number, part).await?);
                    number += 1;
                }
            }
            if seen != len {
                anyhow::bail!(
                    "s3.upload_failed: stream produced {seen} bytes but the body declared {len}"
                );
            }
            if !buffer.is_empty() || parts.is_empty() {
                let part = std::mem::take(&mut buffer);
                parts.push(self.upload_part(key, &upload_id, number, part).await?);
            }
            self.complete_multipart(key, &upload_id, &parts).await
        }
        .await;

        match result {
            Ok(()) => {}
            Err(e) => {
                // Never leave a half-finished multipart upload holding storage.
                self.abort_multipart(key, &upload_id).await;
                return Err(e);
            }
        }

        let full_md5: [u8; 16] = hasher.finalize().into();
        let mut entry = self.head(key).await?;
        // The multipart object's ETag is not a content digest, so `head` cannot
        // report one. Substitute the digest the server verified part-by-part and
        // confirmed the composition of (see the module docs).
        entry.md5 = Some(full_md5);
        Ok(entry)
    }

    async fn create_multipart(
        &self,
        key: &str,
        mime: &str,
        props: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        // S3 fixes user metadata at CreateMultipartUpload time, which is why the
        // per-object content md5 cannot be stamped for this path.
        let mut headers = Self::metadata_headers(props, None)?;
        headers.push(("content-type".to_string(), mime.to_string()));
        let mut action =
            actions::CreateMultipartUpload::new(&self.bucket, Some(&self.credentials), key);
        for (k, v) in &headers {
            action.headers_mut().insert(k.clone(), v.clone());
        }
        let url = action.sign(SIGN_EXPIRY);
        let resp = self
            .execute(&self.http, reqwest::Method::POST, url, &headers, None)
            .await?;
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
        let parsed = actions::CreateMultipartUpload::parse_response(&text).map_err(|e| {
            anyhow::anyhow!("s3.upload_failed: could not parse CreateMultipartUpload: {e}")
        })?;
        let upload_id = parsed.upload_id().to_string();
        // Claim the id for as long as this process is working on it, so the
        // abandoned-upload sweep cannot abort an upload that is very much alive
        // (issue #222).
        self.owned_uploads.lock().insert(upload_id.clone());
        Ok(upload_id)
    }

    /// Release an upload id this process no longer owns (it completed, or it
    /// was aborted). Idempotent.
    fn release_upload(&self, upload_id: &str) {
        self.owned_uploads.lock().remove(upload_id);
    }

    /// Upload one part with its own `Content-MD5`, and verify the returned ETag
    /// is that same digest.
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u16,
        body: Vec<u8>,
    ) -> anyhow::Result<PartRecord> {
        let size = body.len() as u64;
        let md5 = md5_of(&body);
        let headers = vec![(
            "content-md5".to_string(),
            base64::engine::general_purpose::STANDARD.encode(md5),
        )];
        let mut action = actions::UploadPart::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            number,
            upload_id,
        );
        for (k, v) in &headers {
            action.headers_mut().insert(k.clone(), v.clone());
        }
        let url = action.sign(SIGN_EXPIRY);
        let resp = self
            .execute(
                &self.http_stream,
                reqwest::Method::PUT,
                url,
                &headers,
                Some(Bytes::from(body)),
            )
            .await?;
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_default();
        match md5_from_etag(&etag) {
            Some(server) if server == md5 => Ok(PartRecord { number, md5, size }),
            _ => Err(anyhow::Error::new(DriveError::ChecksumMismatch {
                stranded_file_id: None,
            })),
        }
    }

    /// Complete the upload and verify the composed ETag.
    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[PartRecord],
    ) -> anyhow::Result<()> {
        // CompleteMultipartUpload is order-sensitive: the composed ETag - and
        // the object itself - is assembled in the order the parts are listed.
        // Sort by part number rather than trusting the caller's push order.
        let mut parts: Vec<PartRecord> = parts.to_vec();
        parts.sort_by_key(|p| p.number);
        let etags: Vec<String> = parts
            .iter()
            .map(|p| format!("\"{}\"", hex::encode(p.md5)))
            .collect();
        // Whatever the outcome below, this process stops owning the id: a
        // completed upload no longer exists, and a failed completion is handled
        // by the caller (abort, or a fresh session) rather than by us.
        self.release_upload(upload_id);
        let action = actions::CompleteMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = action.sign(SIGN_EXPIRY);
        let body = action.body();
        let headers = vec![("content-type".to_string(), "application/xml".to_string())];
        let resp = self
            .execute(
                &self.http,
                reqwest::Method::POST,
                url,
                &headers,
                Some(Bytes::from(body)),
            )
            .await?;
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;

        // S3 can report a 200 with an error document in the body for
        // CompleteMultipartUpload (the connection is held open while the parts
        // are assembled), so the body must be inspected even on success.
        if text.contains("<Error") {
            return Err(anyhow::Error::new(s3_error_from_response(
                200,
                text.as_bytes(),
                None,
            )));
        }

        let expected = composed_etag(&parts);
        match extract_tag(&text, "ETag") {
            Some(actual) if actual.trim_matches('"') == expected => Ok(()),
            other => {
                // The server assembled something other than the parts we sent.
                // Delete it so the failure leaves nothing behind.
                let stranded = match self.delete_key(key).await {
                    Ok(()) => None,
                    Err(_) => Some(key.to_string()),
                };
                tracing::warn!(
                    target: crate::TARGET,
                    expected = %expected,
                    actual = ?other,
                    "multipart ETag did not match the composed part digests"
                );
                Err(anyhow::Error::new(DriveError::ChecksumMismatch {
                    stranded_file_id: stranded,
                }))
            }
        }
    }

    /// Best-effort `AbortMultipartUpload`, so a failed upload never leaves parts
    /// accruing storage charges.
    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        self.release_upload(upload_id);
        if let Err(err) = self.abort_multipart_strict(key, upload_id).await {
            tracing::warn!(
                target: crate::TARGET,
                %err,
                "failed to abort a multipart upload; its parts may linger until the next startup sweep or a bucket lifecycle rule reaps them"
            );
        }
    }

    /// `AbortMultipartUpload`, surfacing the failure instead of logging it.
    ///
    /// A gone upload id is SUCCESS: the desired state is "this upload holds no
    /// parts", and an upload that never existed (or was already aborted) is
    /// already there. That idempotence is what lets the sweep and the
    /// abandon-session hook both fire at the same id without one of them
    /// reporting a spurious failure.
    async fn abort_multipart_strict(&self, key: &str, upload_id: &str) -> anyhow::Result<()> {
        self.release_upload(upload_id);
        let action = actions::AbortMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            upload_id,
        );
        let url = action.sign(SIGN_EXPIRY);
        match self
            .execute(&self.http, reqwest::Method::DELETE, url, &[], None)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `DeleteObject`. A missing key is success.
    async fn delete_key(&self, key: &str) -> anyhow::Result<()> {
        let action = actions::DeleteObject::new(&self.bucket, Some(&self.credentials), key);
        let url = action.sign(SIGN_EXPIRY);
        match self
            .execute_retrying(&self.http, reqwest::Method::DELETE, url, Vec::new())
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Collect a declared-length stream into memory. Guards against a producer
    /// that overruns its declared length, so a bug upstream cannot exhaust RAM.
    async fn collect_stream(
        len: u64,
        mut stream: Box<dyn futures::Stream<Item = anyhow::Result<Bytes>> + Send + Unpin>,
    ) -> anyhow::Result<Bytes> {
        let mut buf = Vec::with_capacity(len as usize);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if buf.len() as u64 + chunk.len() as u64 > len {
                anyhow::bail!("s3.upload_failed: stream produced more bytes than it declared");
            }
            buf.extend_from_slice(&chunk);
        }
        if buf.len() as u64 != len {
            anyhow::bail!(
                "s3.upload_failed: stream produced {} bytes but the body declared {len}",
                buf.len()
            );
        }
        Ok(Bytes::from(buf))
    }

    /// Route an [`UploadBody`] to the single-PUT or multipart path by size.
    async fn upload_body(
        &self,
        key: &str,
        mime: &str,
        body: UploadBody,
        props: &HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        match body {
            UploadBody::Bytes(bytes) => {
                if bytes.len() as u64 >= MULTIPART_THRESHOLD {
                    let len = bytes.len() as u64;
                    let stream = Box::new(futures::stream::iter(vec![Ok(bytes)]));
                    self.multipart_stream(key, mime, len, props, stream).await
                } else {
                    self.put_object(key, mime, bytes, props).await
                }
            }
            UploadBody::Stream { len, stream } => {
                if len >= MULTIPART_THRESHOLD {
                    self.multipart_stream(key, mime, len, props, stream).await
                } else {
                    // Below the threshold the whole body fits comfortably in
                    // memory (the executor only streams the 4-5 MiB band here),
                    // and buffering buys the server-verified `Content-MD5` that
                    // a single PUT gives us.
                    let bytes = Self::collect_stream(len, stream).await?;
                    self.put_object(key, mime, bytes, props).await
                }
            }
        }
    }

    // -- server-side copy (issue #220 version store) -------------------------

    /// The `x-amz-copy-source` header value naming `src_key` in this bucket.
    ///
    /// S3 wants `/<bucket>/<url-encoded key>` regardless of how the endpoint is
    /// addressed, so the encoding is borrowed from `Bucket::object_url` (the
    /// same encoder every other request path uses) and the bucket name is put
    /// back on the front explicitly.
    fn copy_source_header(&self, src_key: &str) -> anyhow::Result<String> {
        let url = self
            .bucket
            .object_url(src_key)
            .map_err(|e| anyhow::anyhow!("s3.copy_failed: {src_key:?} is not a valid key: {e}"))?;
        let path = url.path();
        let name = self.bucket.name();
        // Path-style urls already carry `/<bucket>` in front of the key; a
        // virtual-host url does not.
        let key_path = if self.path_style {
            path.strip_prefix(&format!("/{name}")).unwrap_or(path)
        } else {
            path
        };
        Ok(format!("/{name}{key_path}"))
    }

    /// Server-side `CopyObject` of `src_key` onto `dest_key`, metadata and all.
    ///
    /// No bytes cross the wire: the whole point of archiving a superseded
    /// version this way is that it costs one request rather than a download plus
    /// an upload. The copied object keeps the source's user metadata (S3's
    /// default `COPY` metadata directive), so the archive carries the same
    /// `driven.*` identity stamp - which is what lets a later reader tell whose
    /// version it is.
    async fn copy_object(&self, src_key: &str, dest_key: &str) -> anyhow::Result<()> {
        let copy_source = self.copy_source_header(src_key)?;
        let headers = vec![("x-amz-copy-source".to_string(), copy_source)];
        // `PutObject` signs exactly the request `CopyObject` is - a PUT at the
        // destination key - and rusty-s3 has no `CopyObject` action of its own,
        // so the copy-source header is what turns one into the other.
        let mut action = actions::PutObject::new(&self.bucket, Some(&self.credentials), dest_key);
        for (k, v) in &headers {
            action.headers_mut().insert(k.clone(), v.clone());
        }
        let url = action.sign(SIGN_EXPIRY);
        let resp = self
            .execute(&self.http, reqwest::Method::PUT, url, &headers, None)
            .await?;
        // Like `CompleteMultipartUpload`, `CopyObject` can answer 200 with an
        // error document in the body while the copy runs, so a 2xx alone is not
        // proof of success.
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
        if text.contains("<Error") {
            return Err(anyhow::Error::new(s3_error_from_response(
                200,
                text.as_bytes(),
                None,
            )));
        }
        Ok(())
    }

    /// Copy an object too large for a single `CopyObject` (over 5 GiB), by
    /// ranged `UploadPartCopy` requests into a fresh multipart upload.
    ///
    /// Still server-side: each part names a byte range of the source and the
    /// bytes never reach this process.
    async fn multipart_copy(
        &self,
        src_key: &str,
        dest_key: &str,
        size: u64,
        mime: &str,
        props: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let copy_source = self.copy_source_header(src_key)?;
        let upload_id = self.create_multipart(dest_key, mime, props).await?;
        let result = async {
            let mut parts: Vec<String> = Vec::new();
            let mut offset: u64 = 0;
            let mut number: u16 = 1;
            while offset < size {
                let end = (offset + COPY_PART_SIZE).min(size) - 1;
                let etag = self
                    .upload_part_copy(dest_key, &upload_id, number, &copy_source, offset, end)
                    .await?;
                parts.push(etag);
                offset = end + 1;
                number += 1;
            }
            self.complete_copied_parts(dest_key, &upload_id, &parts)
                .await
        }
        .await;
        if result.is_err() {
            self.abort_multipart(dest_key, &upload_id).await;
        }
        result
    }

    /// One `UploadPartCopy`: a ranged server-side copy into part `number` of
    /// `upload_id`. Returns the part's ETag as the server reported it.
    async fn upload_part_copy(
        &self,
        dest_key: &str,
        upload_id: &str,
        number: u16,
        copy_source: &str,
        start: u64,
        end_inclusive: u64,
    ) -> anyhow::Result<String> {
        let headers = vec![
            ("x-amz-copy-source".to_string(), copy_source.to_string()),
            (
                "x-amz-copy-source-range".to_string(),
                format!("bytes={start}-{end_inclusive}"),
            ),
        ];
        // `UploadPart` signs the same URL `UploadPartCopy` uses
        // (`PUT <key>?partNumber=N&uploadId=X`); the two copy headers are the
        // only difference.
        let mut action = actions::UploadPart::new(
            &self.bucket,
            Some(&self.credentials),
            dest_key,
            number,
            upload_id,
        );
        for (k, v) in &headers {
            action.headers_mut().insert(k.clone(), v.clone());
        }
        let url = action.sign(SIGN_EXPIRY);
        let resp = self
            .execute(&self.http, reqwest::Method::PUT, url, &headers, None)
            .await?;
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
        if text.contains("<Error") {
            return Err(anyhow::Error::new(s3_error_from_response(
                200,
                text.as_bytes(),
                None,
            )));
        }
        extract_tag(&text, "ETag").ok_or_else(|| {
            anyhow::anyhow!("s3.copy_failed: UploadPartCopy response carried no ETag")
        })
    }

    /// Complete a multipart upload whose parts came from `UploadPartCopy`.
    ///
    /// Unlike [`Self::complete_multipart`] there is no composed-ETag check to
    /// make: the part digests are the SERVER's (this process never saw the
    /// bytes), so re-deriving the composition from them would only prove the
    /// server agrees with itself. The caller verifies the archive by size
    /// instead, against the source object's own recorded size.
    async fn complete_copied_parts(
        &self,
        dest_key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> anyhow::Result<()> {
        self.release_upload(upload_id);
        let action = actions::CompleteMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            dest_key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = action.sign(SIGN_EXPIRY);
        let body = action.body();
        let headers = vec![("content-type".to_string(), "application/xml".to_string())];
        let resp = self
            .execute(
                &self.http,
                reqwest::Method::POST,
                url,
                &headers,
                Some(Bytes::from(body)),
            )
            .await?;
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
        if text.contains("<Error") {
            return Err(anyhow::Error::new(s3_error_from_response(
                200,
                text.as_bytes(),
                None,
            )));
        }
        Ok(())
    }

    // -- abandoned multipart uploads (issue #222) ----------------------------

    /// Aborts every multipart upload under this store's prefix that no live
    /// session can still be using.
    ///
    /// ## Why this exists
    ///
    /// S3 and R2 bill for the parts of an incomplete multipart upload until it
    /// is aborted or a bucket lifecycle rule expires it. Driven's streaming
    /// upload path opens a fresh `CreateMultipartUpload` per attempt, so before
    /// this sweep (and the abandon-on-restart hook that feeds it) a run of
    /// failed attempts left one billed, invisible upload behind each time -
    /// invisible because `ListObjectsV2` does not show them. `driven-localfs`
    /// has always swept its equivalent (abandoned temp files) at construction;
    /// this is the S3 half of that symmetry.
    ///
    /// ## What it will not touch
    ///
    /// Three guards, all of them load-bearing, because an over-eager abort
    /// destroys a transfer in progress:
    ///
    /// 1. **Prefix.** Only uploads whose key is under this store's configured
    ///    root prefix. A bucket Driven shares with another application (or with
    ///    a second Driven destination) keeps its own uploads.
    /// 2. **Ownership.** Never an upload id THIS process opened - see
    ///    [`S3Store::owned_uploads`].
    /// 3. **Age.** Never an upload younger than [`ABANDONED_UPLOAD_MIN_AGE_MS`],
    ///    which is deliberately longer than the 6 days Driven will keep trying
    ///    to resume a persisted session for. An upload older than that can no
    ///    longer be resumed by anything, so aborting it cannot lose work.
    ///
    /// Returns how many uploads were aborted. Errors from an individual abort
    /// are logged and skipped (the next sweep retries); an error ENUMERATING is
    /// returned, because a partial listing would understate the leak.
    pub async fn sweep_abandoned_multipart_uploads(&self) -> anyhow::Result<usize> {
        let now = now_ms();
        let uploads = self.list_multipart_uploads().await?;
        let mut aborted = 0usize;
        for upload in uploads {
            let owned = self.owned_uploads.lock().contains(&upload.upload_id);
            if !is_sweepable(&upload, &self.prefix, owned, now) {
                continue;
            }
            match self
                .abort_multipart_strict(&upload.key, &upload.upload_id)
                .await
            {
                Ok(()) => aborted += 1,
                Err(err) => tracing::warn!(
                    target: crate::TARGET,
                    key = %upload.key,
                    %err,
                    "could not abort an abandoned multipart upload; a later sweep will retry"
                ),
            }
        }
        if aborted > 0 {
            tracing::info!(
                target: crate::TARGET,
                aborted,
                "aborted abandoned multipart uploads that were still holding parts"
            );
        }
        Ok(aborted)
    }

    /// Page through `ListMultipartUploads` for the whole bucket.
    ///
    /// Bucket-wide rather than prefix-scoped: the `prefix` parameter is
    /// applied by the caller instead, so an upload whose key sits just outside
    /// the configured prefix still shows up in the listing and is then visibly
    /// skipped, rather than being silently invisible to the sweep.
    async fn list_multipart_uploads(&self) -> anyhow::Result<Vec<InFlightUpload>> {
        let mut out = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut upload_marker: Option<String> = None;
        loop {
            // rusty-s3 models no `ListMultipartUploads` action. `GetObject`
            // against the EMPTY object name signs exactly the request this needs
            // - a GET on the bucket url - and the `uploads` subresource in the
            // query is what selects the multipart listing.
            let mut action = actions::GetObject::new(&self.bucket, Some(&self.credentials), "");
            action.query_mut().insert("uploads", "");
            action
                .query_mut()
                .insert("max-uploads", UPLOAD_LIST_PAGE_SIZE.to_string());
            if let Some(k) = key_marker.clone() {
                action.query_mut().insert("key-marker", k);
            }
            if let Some(u) = upload_marker.clone() {
                action.query_mut().insert("upload-id-marker", u);
            }
            let url = action.sign(SIGN_EXPIRY);
            let resp = self
                .execute_retrying(&self.http, reqwest::Method::GET, url, Vec::new())
                .await?;
            let text = resp
                .text()
                .await
                .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
            let page = parse_multipart_uploads(&text);
            out.extend(page.uploads);
            if !page.truncated {
                return Ok(out);
            }
            // A truncated page with no markers would loop forever; treat it as
            // the end and report what was collected.
            match (page.next_key_marker, page.next_upload_id_marker) {
                (Some(k), Some(u)) => {
                    key_marker = Some(k);
                    upload_marker = Some(u);
                }
                _ => return Ok(out),
            }
        }
    }
}

/// One multipart upload as `ListMultipartUploads` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InFlightUpload {
    key: String,
    upload_id: String,
    /// When the upload was initiated, Unix epoch ms, or `None` when the server
    /// reported no parseable `Initiated` timestamp.
    ///
    /// `None` means the sweep SKIPS it. An undateable upload could be seconds
    /// old, and the cost of guessing wrong in that direction is a destroyed
    /// transfer, against a leak that stays visible and can be reaped by a bucket
    /// lifecycle rule.
    initiated_ms: Option<i64>,
}

/// The parsed shape of one `ListMultipartUploads` page.
#[derive(Debug, Default)]
struct MultipartUploadPage {
    uploads: Vec<InFlightUpload>,
    truncated: bool,
    next_key_marker: Option<String>,
    next_upload_id_marker: Option<String>,
}

// -- helpers -----------------------------------------------------------------

fn md5_of(bytes: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(bytes);
    h.finalize().into()
}

/// Parse a 32-char hex md5.
fn parse_md5_hex(s: &str) -> Option<[u8; 16]> {
    let bytes = hex::decode(s.trim().trim_matches('"')).ok()?;
    bytes.try_into().ok()
}

/// The content md5 an ETag carries, if it carries one.
///
/// A single-part object's ETag IS the content md5. A MULTIPART object's ETag is
/// `md5(concat(part md5s))-N` - a digest OF DIGESTS, not of the content - so the
/// `-N` form deliberately yields `None` rather than a plausible-looking wrong
/// answer.
fn md5_from_etag(etag: &str) -> Option<[u8; 16]> {
    let e = etag.trim().trim_matches('"');
    if e.contains('-') {
        return None;
    }
    parse_md5_hex(e)
}

/// The ETag S3 must report for an object assembled from `parts`:
/// `hex(md5(concat(part md5 bytes)))-<count>`.
fn composed_etag(parts: &[PartRecord]) -> String {
    let mut h = Md5::new();
    for p in parts {
        h.update(p.md5);
    }
    let digest: [u8; 16] = h.finalize().into();
    format!("{}-{}", hex::encode(digest), parts.len())
}

/// Pull the text of the first `<tag>..</tag>` out of an XML document,
/// XML-unescaping the result.
///
/// The unescape is load-bearing, not decoration: an S3 ETag is a QUOTED string,
/// and servers escape those quotes differently in the
/// `CompleteMultipartUpload` response - MinIO emits the numeric entity `&#34;`
/// and Cloudflare R2 the named `&quot;`. Comparing the raw text against the
/// composed digest therefore failed on BOTH servers even though the digest was
/// right, which would have turned every multipart upload into a spurious
/// checksum mismatch.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(xml[start..end].trim()))
}

/// Decode the five predefined XML entities plus their numeric forms.
///
/// A full XML parser is not warranted for reading one ETag out of one response,
/// and `instant_xml` (which rusty-s3 uses for the responses it models) does not
/// expose a standalone unescape.
fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        // `&amp;` last: decoding it first would let `&amp;quot;` (a literal
        // "&quot;" in the document) wrongly become a quote.
        .replace("&amp;", "&")
}

/// Whether the sweep may abort `upload` (issue #222).
///
/// Split out from [`S3Store::sweep_abandoned_multipart_uploads`] because getting
/// it wrong destroys a transfer in progress, and that deserves a test that needs
/// no server. All three guards must pass:
///
/// 1. **Prefix** - the key is under this store's configured root. A bucket
///    shared with another application (or a second Driven destination) keeps its
///    own uploads.
/// 2. **Ownership** - not an upload id THIS process opened.
/// 3. **Age** - older than [`ABANDONED_UPLOAD_MIN_AGE_MS`], which is longer than
///    the 6 days Driven will keep trying to resume a persisted session for, so
///    an upload past it can no longer be resumed by anything. An upload the
///    server did not date is skipped: it could be seconds old, and guessing
///    wrong in that direction costs a live transfer.
fn is_sweepable(upload: &InFlightUpload, prefix: &str, owned: bool, now_ms: i64) -> bool {
    if !upload.key.starts_with(prefix) || owned {
        return false;
    }
    match upload.initiated_ms {
        Some(initiated) => now_ms.saturating_sub(initiated) >= ABANDONED_UPLOAD_MIN_AGE_MS,
        None => false,
    }
}

/// Parse a `ListMultipartUploads` response.
///
/// Hand-rolled for the same reason [`extract_tag`] is: rusty-s3 models no
/// `ListMultipartUploads` action, and pulling an XML parser in to read three
/// fields out of one response would not make it more correct. Every field is
/// XML-unescaped through the shared helper, so an upload id containing `&` (S3
/// upload ids are opaque base64-ish blobs and legitimately can) round-trips
/// rather than being truncated into an id the abort would miss.
fn parse_multipart_uploads(xml: &str) -> MultipartUploadPage {
    let mut page = MultipartUploadPage {
        truncated: extract_tag(xml, "IsTruncated").as_deref() == Some("true"),
        next_key_marker: extract_tag(xml, "NextKeyMarker").filter(|s| !s.is_empty()),
        next_upload_id_marker: extract_tag(xml, "NextUploadIdMarker").filter(|s| !s.is_empty()),
        ..MultipartUploadPage::default()
    };
    let mut rest = xml;
    while let Some(start) = rest.find("<Upload>") {
        let after = &rest[start + "<Upload>".len()..];
        let Some(end) = after.find("</Upload>") else {
            break;
        };
        let block = &after[..end];
        rest = &after[end..];
        let (Some(key), Some(upload_id)) =
            (extract_tag(block, "Key"), extract_tag(block, "UploadId"))
        else {
            continue;
        };
        if key.is_empty() || upload_id.is_empty() {
            continue;
        }
        page.uploads.push(InFlightUpload {
            key,
            upload_id,
            initiated_ms: extract_tag(block, "Initiated")
                .as_deref()
                .and_then(parse_iso8601_ms),
        });
    }
    page
}

/// Parse an ISO 8601 UTC instant (`2010-11-10T20:48:33.000Z`) into epoch ms.
///
/// Only the shape S3 emits is accepted; anything else yields `None`, which the
/// sweep reads as "do not touch this upload".
fn parse_iso8601_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Trim the zone marker and any fractional seconds; S3 always reports UTC.
    let time = rest.trim_end_matches('Z');
    let time = time.split_once('.').map_or(time, |(hms, _)| hms);
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next()?.parse().ok()?;
    Some((days_from_civil(year, month, day) * 86_400 + hour * 3_600 + min * 60 + sec) * 1_000)
}

/// Days since the Unix epoch for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Exact for the whole calendar, and shared by the two date
/// parsers so they cannot drift apart.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an RFC 1123 HTTP date into Unix epoch ms.
///
/// Hand-rolled rather than pulling a date crate for one header: the format is
/// fixed (`Wed, 21 Oct 2015 07:28:00 GMT`), always UTC, and the value only feeds
/// `RemoteEntry.modified_time`, which Driven displays but never uses for sync
/// decisions (those key off the local mtime and the content hash). An
/// unparseable value degrades to 0 rather than failing the request.
fn parse_http_date_ms(s: &str) -> Option<i64> {
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

    Some((days_from_civil(year, month, day) * 86_400 + hour * 3_600 + min * 60 + sec) * 1_000)
}

/// Encode a multipart handle into the opaque [`ResumableSession::url`] string.
fn encode_session_url(key: &str, upload_id: &str) -> String {
    let json = serde_json::json!({ "key": key, "uploadId": upload_id }).to_string();
    format!(
        "{SESSION_URL_SCHEME}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

/// Decode a [`ResumableSession::url`] minted by [`encode_session_url`].
fn decode_session_url(url: &str) -> anyhow::Result<(String, String)> {
    let encoded = url.strip_prefix(SESSION_URL_SCHEME).ok_or_else(|| {
        anyhow::anyhow!("s3.session_invalid: resumable session was not issued by this backend")
    })?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| anyhow::anyhow!("s3.session_invalid: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_slice(&json).map_err(|e| anyhow::anyhow!("s3.session_invalid: {e}"))?;
    let key = v
        .get("key")
        .and_then(|k| k.as_str())
        .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: no key"))?;
    let upload_id = v
        .get("uploadId")
        .and_then(|k| k.as_str())
        .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: no uploadId"))?;
    Ok((key.to_string(), upload_id.to_string()))
}

// -- the trait ---------------------------------------------------------------

#[async_trait]
impl RemoteStore for S3Store {
    /// Folders are key prefixes, so this synthesizes an entry and issues NO
    /// request: a prefix always exists, there is nothing to create, and nothing
    /// to race. Deliberately no zero-byte marker object either - it would appear
    /// in `list_source_object_ids` as an object Driven owns with no `file_state`
    /// row, and the remote-existence audit would try to heal it forever.
    async fn ensure_folder(
        &self,
        parent_id: &str,
        name: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<RemoteEntry> {
        let id = keys::folder_id(parent_id, name);
        Ok(RemoteEntry {
            name: keys::base_name(&id).to_string(),
            parents: vec![keys::parent_of(&id)],
            id,
            size: None,
            md5: None,
            mime_type: FOLDER_MIME.to_string(),
            modified_time: 0,
            trashed: false,
            app_properties: HashMap::new(),
        })
    }

    /// Direct children of a prefix: `CommonPrefixes` become folder entries,
    /// `Contents` become object entries.
    ///
    /// `app_properties` are NOT populated here - S3's list API does not return
    /// user metadata, and a HEAD per object would turn a picker page into
    /// hundreds of requests. The consumers that need properties
    /// (`find_by_op_uuid`, `list_source_object_ids`) fetch them explicitly; the
    /// picker and the CLI, which are `list_folder`'s only production callers,
    /// need names only.
    async fn list_folder(
        &self,
        folder_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<Vec<RemoteEntry>> {
        let prefix = if folder_id.is_empty() || folder_id.ends_with('/') {
            folder_id.to_string()
        } else {
            format!("{folder_id}/")
        };
        let mut out = Vec::new();
        self.list_pages(&prefix, Some("/"), |page| {
            for cp in &page.common_prefixes {
                // Driven's own version store is not a folder the user picks a
                // backup destination inside; it is internal bookkeeping that
                // happens to live in their bucket.
                if keys::is_version_key(&self.prefix, &cp.prefix) {
                    continue;
                }
                out.push(RemoteEntry {
                    name: keys::base_name(&cp.prefix).to_string(),
                    parents: vec![prefix.clone()],
                    id: cp.prefix.clone(),
                    size: None,
                    md5: None,
                    mime_type: FOLDER_MIME.to_string(),
                    modified_time: 0,
                    trashed: false,
                    app_properties: HashMap::new(),
                });
            }
            for obj in &page.contents {
                // A zero-byte "directory marker" written by another tool has a
                // key equal to the prefix; it is not a child.
                if obj.key == prefix {
                    continue;
                }
                out.push(RemoteEntry {
                    name: keys::base_name(&obj.key).to_string(),
                    parents: vec![prefix.clone()],
                    id: obj.key.clone(),
                    size: Some(obj.size),
                    md5: md5_from_etag(&obj.etag),
                    mime_type: "application/octet-stream".to_string(),
                    modified_time: 0,
                    trashed: false,
                    app_properties: HashMap::new(),
                });
            }
        })
        .await?;
        Ok(out)
    }

    /// PUT a new object at `<parent_id><name>`.
    ///
    /// Unlike Drive, S3 keys are unique: a `create` against an existing key
    /// OVERWRITES rather than producing a duplicate. That is strictly safer than
    /// the semantics the trait documents, and the executor's caller-side
    /// "do not create over an existing `file_state.drive_file_id`" discipline
    /// still holds.
    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let key = keys::join_key(parent_id, name);
        self.upload_body(&key, mime, body, &app_properties).await
    }

    /// Overwrite the object at `file_id` (which IS its key).
    ///
    /// The existing object is read first so its properties and content type can
    /// be carried forward (S3 metadata is replace-only, so a patch that named
    /// one key would otherwise drop the rest of the identity stamp).
    ///
    /// ## A missing target is NOT an error here
    ///
    /// The read is `.ok()`-swallowed on purpose. On Drive a `file_id` is an
    /// opaque handle: once the object is gone the id can never be revived, so
    /// the executor has a dedicated `update_target_is_missing` self-heal that
    /// clears the stale id and re-plans a create. An S3 key is NOT opaque - it
    /// is derived from the relative path, and writing to it revives it. So an
    /// update against a deleted key correctly RE-CREATES the object at exactly
    /// the key the re-plan would have chosen, leaving
    /// `file_state.drive_file_id` valid. Surfacing a 404 here would send the
    /// executor round a heal cycle to reach the same end state.
    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let existing = self.head(file_id).await.ok();
        let mime = existing
            .as_ref()
            .map(|e| e.mime_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        // Merge onto whatever the object already carried, so a caller that
        // patches one key does not silently drop the rest of the identity stamp.
        let mut props = existing.map(|e| e.app_properties).unwrap_or_default();
        props.extend(app_properties_patch);
        self.upload_body(file_id, &mime, body, &props).await
    }

    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        let (key, props) = match &kind {
            ResumableKind::Create {
                parent_id,
                name,
                app_properties,
            } => (keys::join_key(parent_id, name), app_properties.clone()),
            ResumableKind::Update { file_id } => {
                // Carry the existing object's properties forward: a resumable
                // update re-writes the whole object, and S3 metadata is
                // replace-only.
                let props = self
                    .head(file_id)
                    .await
                    .map(|e| e.app_properties)
                    .unwrap_or_default();
                (file_id.clone(), props)
            }
        };
        let upload_id = self.create_multipart(&key, mime, &props).await?;
        self.uploads
            .lock()
            .insert(upload_id.clone(), MultipartState::new());
        Ok(ResumableSession {
            url: encode_session_url(&key, &upload_id),
            issued_at: now_ms(),
            size,
            kind,
        })
    }

    /// Aborts the multipart upload behind an abandoned session (issue #222).
    ///
    /// Without this, every abandoned attempt left its parts on the bucket -
    /// billed, invisible to `ListObjectsV2`, and never collected. The executor
    /// calls it at the two points a session becomes unreachable (a fresh
    /// session replacing a persisted one, and a discarded invalid session), so
    /// the common case never reaches
    /// [`S3Store::sweep_abandoned_multipart_uploads`] at all.
    ///
    /// A session issued by another backend, or one whose upload is already gone,
    /// is not an error: the desired end state is "these parts are not being
    /// billed for", and both already satisfy it.
    async fn abandon_resumable_session(&self, session: &ResumableSession) -> anyhow::Result<()> {
        let (key, upload_id) = match decode_session_url(&session.url) {
            Ok(pair) => pair,
            // Not ours to abort. Never an error: the executor hands us whatever
            // it had persisted, and a session minted by a different backend
            // simply has nothing for this store to release.
            Err(_) => return Ok(()),
        };
        self.uploads.lock().remove(&upload_id);
        self.abort_multipart_strict(&key, &upload_id).await
    }

    /// Copies the object at `file_id` into Driven's version store so a
    /// subsequent write to that key cannot destroy it (issue #220).
    ///
    /// The copy is SERVER-SIDE (`CopyObject`, or ranged `UploadPartCopy` above
    /// S3's 5 GiB single-copy ceiling): no bytes travel through this process, so
    /// retaining a version costs one request rather than a download plus an
    /// upload.
    ///
    /// The destination key is a pure function of `(file_id, content_token)`, and
    /// an archive that already exists is returned WITHOUT re-copying. Both
    /// halves are load-bearing for crash safety: an op that dies between the
    /// archive and its commit is replayed against the same token, by which time
    /// the live key may already hold the NEW bytes - so a blind re-copy would
    /// overwrite a correct archive with the very content it exists to preserve.
    async fn archive_version(
        &self,
        file_id: &str,
        content_token: &str,
    ) -> anyhow::Result<Option<String>> {
        let dest = keys::version_key(&self.prefix, file_id, content_token);
        if self.head(&dest).await.is_ok() {
            tracing::debug!(
                target: crate::TARGET,
                key = %dest,
                "version already archived; reusing it rather than re-copying"
            );
            return Ok(Some(dest));
        }
        // HEAD the source rather than trusting a caller-supplied size: the copy
        // strategy turns on it, and a 404 here is the honest "there is nothing to
        // preserve" answer rather than an empty archive.
        let src = self.head(file_id).await?;
        let size = src.size.unwrap_or(0);
        if size > MAX_SINGLE_COPY {
            self.multipart_copy(file_id, &dest, size, &src.mime_type, &src.app_properties)
                .await?;
        } else {
            self.copy_object(file_id, &dest).await?;
        }
        // Prove the archive is a whole copy before anyone records a version row
        // pointing at it. A short archive would be a silently truncated
        // "previous version", which is the failure this whole feature exists to
        // prevent.
        let archived = self.head(&dest).await?;
        match archived.size {
            Some(actual) if actual == size => Ok(Some(dest)),
            other => {
                let _ = self.delete_key(&dest).await;
                Err(anyhow::anyhow!(
                    "s3.copy_failed: archived version of {file_id:?} is {other:?} bytes, expected {size}"
                ))
            }
        }
    }

    /// Buffer wire chunks into S3-legal parts and flush them.
    ///
    /// ## The rewind
    ///
    /// The caller's persisted `acked_offset` counts bytes this store ACCEPTED,
    /// including any it had merely buffered when the process died. After a
    /// restart the buffer is gone, so resuming at that offset would silently
    /// drop the buffered bytes from the middle of the object. Instead, the first
    /// call for an upload this process did not open returns
    /// `InProgress { received: 0 }`, which makes the executor replay the body
    /// from the beginning (it re-reads and re-hashes the whole local file on
    /// resume anyway).
    ///
    /// The replay is NOT wasted network: part boundaries are a pure function of
    /// the offset, so each replayed part is compared against the `ListParts`
    /// digest of the part already on the server, and an identical part is
    /// skipped. The bytes are re-read locally and re-hashed; only genuinely
    /// missing parts go over the wire. As a bonus this VERIFIES the pre-crash
    /// parts against the current local bytes instead of trusting them.
    ///
    /// The replay can never assemble a MIXED object out of two different
    /// versions of the file. `CompleteMultipartUpload` lists only the parts
    /// THIS replay produced, and S3 discards any part left over from the
    /// crashed run that the list does not name - so a stale part beyond the
    /// replay's last part number is dropped, not appended. (The executor also
    /// refuses to resume at all unless `payload.resume_identity` still matches
    /// the local file, so the replayed bytes are the same bytes the crashed run
    /// was uploading; the part-digest comparison above then re-proves that
    /// per part.)
    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        let (key, upload_id) = decode_session_url(&session.url)?;

        // Hydrate an upload this process did not open.
        let needs_hydration = !self.uploads.lock().contains_key(&upload_id);
        if needs_hydration {
            let existing = match self.list_parts(&key, &upload_id).await {
                Ok(parts) => parts,
                // A NoSuchUpload (or any definitive failure to enumerate) means
                // the session is dead; the caller restarts from scratch.
                Err(err) => {
                    tracing::warn!(
                        target: crate::TARGET,
                        %err,
                        "could not enumerate the parts of a persisted multipart upload; invalidating the session"
                    );
                    return Ok(ResumeProgress::SessionInvalid);
                }
            };
            let mut state = MultipartState::new();
            state.existing = existing;
            state.rewound = false;
            self.uploads.lock().insert(upload_id.clone(), state);
        }

        // Ask for the rewind (once) if this is a hydrated session that has not
        // replayed yet, and refuse any offset that is not where we are.
        {
            let mut uploads = self.uploads.lock();
            let state = uploads
                .get_mut(&upload_id)
                .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: upload state vanished"))?;
            if let Some(received) = seek_verdict(state, offset) {
                return Ok(ResumeProgress::InProgress { received });
            }
        }

        // Fold the chunk in exactly once, then let the drain loop emit every
        // part it makes flushable.
        let is_final = offset + chunk.len() as u64 >= session.size;
        {
            let mut uploads = self.uploads.lock();
            let state = uploads
                .get_mut(&upload_id)
                .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: upload state vanished"))?;
            if !chunk.is_empty() {
                state.md5.update(&chunk);
                state.buffer.extend_from_slice(&chunk);
            }
        }

        self.drain_and_maybe_complete(session, &key, &upload_id, is_final)
            .await
    }

    /// S3 has NO trash: this permanently deletes the object, exactly like
    /// [`Self::delete_permanent`]. Users who want recoverable deletes enable
    /// bucket versioning, a server-side setting Driven does not manage. A
    /// missing key is success (idempotent).
    ///
    /// ## The one exception: Driven's own version store
    ///
    /// An object under [`keys::VERSIONS_SEGMENT`] is a retained version that
    /// [`Self::archive_version`] put there, and trashing it is a NO-OP.
    ///
    /// This is not a special case bolted on; it is what makes the executor's
    /// Drive-shaped flow correct here. After a versioned change the executor
    /// "trashes" the superseded object, meaning "move it out of the live tree
    /// but keep it restorable" - which is exactly what Drive's trash does, and
    /// exactly what the archive copy ALREADY did. Deleting it instead would
    /// destroy the version the same cycle just recorded. Every path that really
    /// must free an archived version's storage (the count-cap prune) goes
    /// through [`Self::delete_permanent`], which still hard-deletes it.
    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        if keys::is_version_key(&self.prefix, file_id) {
            tracing::debug!(
                target: crate::TARGET,
                key = %file_id,
                "trash of an archived version is a no-op; it already lives in the version store"
            );
            return Ok(());
        }
        self.delete_key(file_id).await
    }

    async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
        self.delete_key(file_id).await
    }

    /// `HeadObject`.
    ///
    /// `md5` is the object's content digest when one is recoverable: the
    /// `driven-md5` metadata stamped by the single-PUT path, else the ETag when
    /// it is a single-part digest. For an object uploaded via MULTIPART it is
    /// `None`, because S3 stores no content digest for one (the ETag is a digest
    /// of part digests) and inventing a plausible value would be worse than
    /// admitting the absence. Nothing in the sync engine verifies against
    /// `metadata().md5`: uploads are verified against the digest returned by the
    /// upload itself, and the deep-verify pass re-hashes locally.
    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        self.head(file_id).await
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        let action = actions::GetObject::new(&self.bucket, Some(&self.credentials), file_id);
        let url = action.sign(SIGN_EXPIRY);
        let resp = self
            .execute(&self.http_stream, reqwest::Method::GET, url, &[], None)
            .await?;
        Ok(DownloadStream(Box::new(BodyReader::new(resp))))
    }

    /// Find an object under `parent_id` carrying `op_uuid`.
    ///
    /// S3 cannot filter a listing by user metadata, so this lists the ONE folder
    /// prefix and HEADs its direct children with bounded concurrency. Scope
    /// keeps that cheap: reconciliation calls it for a single crashed op, on a
    /// single directory.
    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        let prefix = if parent_id.is_empty() || parent_id.ends_with('/') {
            parent_id.to_string()
        } else {
            format!("{parent_id}/")
        };
        let mut keys_in_folder = Vec::new();
        self.list_pages(&prefix, Some("/"), |page| {
            for obj in &page.contents {
                if obj.key != prefix {
                    keys_in_folder.push(obj.key.clone());
                }
            }
        })
        .await?;

        let entries = self.head_all(keys_in_folder).await?;
        let mut matches: Vec<RemoteEntry> = entries
            .into_iter()
            .filter(|e| {
                e.app_properties
                    .get(driven_remote::props::CLIENT_OP_UUID_KEY)
                    .map(|v| v == op_uuid)
                    .unwrap_or(false)
            })
            .collect();
        if matches.len() > 1 {
            tracing::warn!(
                target: crate::TARGET,
                count = matches.len(),
                "multiple objects carry the same client op uuid; adopting the most recent"
            );
            matches.sort_by_key(|e| e.modified_time);
        }
        Ok(matches.pop())
    }

    /// Every LIVE object id belonging to `source_id`.
    ///
    /// ## Cost, and why it is the right trade
    ///
    /// The trait's doc motivates this method as `ceil(N / pageSize)` requests
    /// instead of one metadata GET per file - which relies on the backend being
    /// able to SEARCH by `appProperties`. S3 cannot: `ListObjectsV2` returns
    /// keys, sizes and ETags, never user metadata, so the source id can only be
    /// read with a `HeadObject`. This therefore costs one paged listing PLUS one
    /// HEAD per object under the prefix (at [`HEAD_CONCURRENCY`] in flight).
    ///
    /// The alternatives were all worse: a per-source side-index object would
    /// desync from reality on any crash between the two writes, and encoding the
    /// source id into the key layout is impossible because `ensure_folder` never
    /// receives `app_properties`. A slow-but-correct audit beats a fast one that
    /// can be wrong, and this runs on the deep-verify cadence (weekly by
    /// default), not per backup cycle.
    ///
    /// ## Completeness
    ///
    /// The caller heals `recorded - live`, so a truncated result would read as a
    /// mass deletion. Every page failure and every HEAD failure propagates as
    /// `Err`; this never returns a partial set.
    ///
    /// ## Archived versions are deliberately absent
    ///
    /// The set answers "does this recorded object still exist", and the recorded
    /// population is `file_state` + bundles - never a `file_versions` row. An
    /// archived version is therefore not something any caller looks up here, and
    /// including it would cost one extra `HeadObject` per retained version on
    /// every audit and every scrub. That mirrors Drive, where a superseded
    /// version sits in the trash and this listing excludes trashed objects.
    async fn list_source_object_ids(
        &self,
        source_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<HashSet<String>> {
        let mut all_keys = Vec::new();
        self.list_pages(&self.prefix, None, |page| {
            for obj in &page.contents {
                // Skip any zero-byte directory marker another tool left behind.
                if obj.key.ends_with('/') {
                    continue;
                }
                if keys::is_version_key(&self.prefix, &obj.key) {
                    continue;
                }
                all_keys.push(obj.key.clone());
            }
        })
        .await?;

        let entries = self.head_all(all_keys).await?;
        Ok(entries
            .into_iter()
            .filter(|e| {
                e.app_properties
                    .get(driven_remote::props::SOURCE_ID_KEY)
                    .map(|v| v == source_id)
                    .unwrap_or(false)
            })
            .map(|e| e.id)
            .collect())
    }

    /// Bucket usage.
    ///
    /// S3 exposes no quota API, so `limit` is `None` (unlimited, which is the
    /// truth for an object store billed by usage) and the usage figures are the
    /// summed sizes of the objects under Driven's prefix, from one paged
    /// listing. `usage_in_drive_trash` is always 0: S3 has no trash.
    async fn about(&self) -> anyhow::Result<AboutInfo> {
        let mut usage: u64 = 0;
        self.list_pages(&self.prefix, None, |page| {
            for obj in &page.contents {
                usage = usage.saturating_add(obj.size);
            }
        })
        .await?;
        Ok(AboutInfo {
            limit: None,
            usage,
            usage_in_drive: usage,
            usage_in_drive_trash: 0,
        })
    }
}

impl S3Store {
    /// Enumerate the parts already committed for `upload_id`.
    async fn list_parts(
        &self,
        key: &str,
        upload_id: &str,
    ) -> anyhow::Result<HashMap<u16, (String, u64)>> {
        let mut out = HashMap::new();
        let mut marker: Option<u16> = None;
        loop {
            let mut action =
                actions::ListParts::new(&self.bucket, Some(&self.credentials), key, upload_id);
            if let Some(m) = marker {
                action.set_part_number_marker(m);
            }
            let url = action.sign(SIGN_EXPIRY);
            let resp = self
                .execute_retrying(&self.http, reqwest::Method::GET, url, Vec::new())
                .await?;
            let text = resp
                .text()
                .await
                .map_err(|e| anyhow::Error::new(DriveError::from_transport(e)))?;
            let parsed = actions::ListParts::parse_response(&text).map_err(|e| {
                anyhow::anyhow!("s3.session_invalid: could not parse ListParts: {e}")
            })?;
            for p in &parsed.parts {
                out.insert(p.number, (p.etag.trim_matches('"').to_string(), p.size));
            }
            match parsed.next_part_number_marker {
                Some(m) => marker = Some(m),
                None => return Ok(out),
            }
        }
    }

    /// Upload a part, or skip the network entirely when an identical part is
    /// already committed on the server from a previous run.
    async fn flush_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u16,
        bytes: Vec<u8>,
    ) -> anyhow::Result<PartRecord> {
        let md5 = md5_of(&bytes);
        let size = bytes.len() as u64;
        let already = {
            let uploads = self.uploads.lock();
            uploads
                .get(upload_id)
                .and_then(|s| s.existing.get(&number).cloned())
        };
        if part_already_uploaded(already.as_ref(), &md5, size) {
            tracing::debug!(
                target: crate::TARGET,
                number,
                "part already on the server with a matching digest; skipping the upload"
            );
            return Ok(PartRecord { number, md5, size });
        }
        self.upload_part(key, upload_id, number, bytes).await
    }

    /// Drain any remaining full parts and, when the body is done, complete the
    /// upload.
    async fn drain_and_maybe_complete(
        &self,
        session: &ResumableSession,
        key: &str,
        upload_id: &str,
        is_final: bool,
    ) -> anyhow::Result<ResumeProgress> {
        loop {
            let flush = {
                let mut uploads = self.uploads.lock();
                let state = uploads
                    .get_mut(upload_id)
                    .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: upload state vanished"))?;
                take_flushable(state, is_final)
            };
            let Some((number, bytes)) = flush else {
                break;
            };
            let record = self.flush_part(key, upload_id, number, bytes).await?;
            let mut uploads = self.uploads.lock();
            let state = uploads
                .get_mut(upload_id)
                .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: upload state vanished"))?;
            state.buffer_start += record.size;
            state.parts.push(record);
        }

        let (consumed, parts, full_md5) = {
            let uploads = self.uploads.lock();
            let state = uploads
                .get(upload_id)
                .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: upload state vanished"))?;
            (
                state.consumed(),
                state.parts.clone(),
                state.md5.clone().finalize().into(),
            )
        };

        if !is_final || consumed < session.size {
            return Ok(ResumeProgress::InProgress { received: consumed });
        }

        let full_md5: [u8; 16] = full_md5;
        match self.complete_multipart(key, upload_id, &parts).await {
            Ok(()) => {
                self.uploads.lock().remove(upload_id);
                let mut entry = self.metadata(key).await?;
                entry.md5 = Some(full_md5);
                Ok(ResumeProgress::Completed(entry))
            }
            Err(e) => {
                self.uploads.lock().remove(upload_id);
                // A completion failure kills the session: the caller must not
                // keep pushing chunks at it. Log the cause - the caller only
                // sees an opaque `SessionInvalid`, so without this the reason a
                // large upload restarts would be invisible.
                if is_session_fatal(&e) {
                    tracing::warn!(
                        target: crate::TARGET,
                        err = %e,
                        "multipart completion failed fatally; invalidating the session"
                    );
                    self.abort_multipart(key, upload_id).await;
                    return Ok(ResumeProgress::SessionInvalid);
                }
                Err(e)
            }
        }
    }
}

/// Decide where the caller must be before its chunk can be accepted.
///
/// `Some(received)` tells the caller to seek to `received` and re-send from
/// there; `None` means the chunk lines up with what this store has consumed and
/// can be folded in.
///
/// Two rules, in order:
///
/// 1. **The one-shot rewind.** A state hydrated from a PERSISTED session has an
///    empty buffer, but the caller's persisted offset counts bytes this store
///    ACCEPTED - including any that were only buffered when the process died.
///    Resuming there would drop those bytes out of the middle of the object, so
///    the first chunk after hydration is answered with `0`: replay from the
///    start. Bounded to once (`rewound`), so a caller that ignores the rewind
///    cannot make this spin.
/// 2. **No gaps, no overlaps.** Any other mismatch is answered with the true
///    consumed offset rather than accepted, because writing a chunk at the wrong
///    position would corrupt the assembled object silently.
fn seek_verdict(state: &mut MultipartState, offset: u64) -> Option<u64> {
    if !state.rewound {
        state.rewound = true;
        if offset != 0 {
            return Some(0);
        }
    }
    let consumed = state.consumed();
    if offset != consumed {
        return Some(consumed);
    }
    None
}

/// Whether a part re-derived during a replay is ALREADY on the server, and so
/// can skip the upload.
///
/// Both the size AND the digest must match. The digest alone is not enough: S3
/// part ETags are md5s, and accepting one without checking the size would let a
/// truncated part masquerade as complete. This is also what turns the replay
/// into a verification of the pre-crash parts rather than a blind trust of them.
fn part_already_uploaded(existing: Option<&(String, u64)>, md5: &[u8; 16], size: u64) -> bool {
    match existing {
        Some((etag, existing_size)) => *existing_size == size && parse_md5_hex(etag) == Some(*md5),
        None => false,
    }
}

/// Take the next flushable part out of `state`, if any: a full [`PART_SIZE`]
/// buffer, or - once the body is complete - whatever remains.
fn take_flushable(state: &mut MultipartState, is_final: bool) -> Option<(u16, Vec<u8>)> {
    let number = (state.buffer_start / PART_SIZE as u64) as u16 + 1;
    if state.buffer.len() >= PART_SIZE {
        let rest = state.buffer.split_off(PART_SIZE);
        let part = std::mem::replace(&mut state.buffer, rest);
        return Some((number, part));
    }
    if is_final && (!state.buffer.is_empty() || state.parts.is_empty()) {
        let part = std::mem::take(&mut state.buffer);
        return Some((number, part));
    }
    None
}

/// Whether an error means the multipart session can never make progress again
/// (so the caller must discard it rather than retry against it).
fn is_session_fatal(err: &anyhow::Error) -> bool {
    match driven_remote::classification_of(err) {
        Some(driven_remote::remote_store::DriveErrorClassification::Transient5xx)
        | Some(driven_remote::remote_store::DriveErrorClassification::Network)
        | Some(driven_remote::remote_store::DriveErrorClassification::RateLimited { .. }) => false,
        // A checksum mismatch, a dead upload id, or an auth failure will not fix
        // itself by pushing more chunks at the same session.
        _ => true,
    }
}

/// Wall-clock now in Unix epoch ms.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_yields_a_content_md5_only_for_single_part_objects() {
        let hexed = "9a0364b9e99bb480dd25e1f0284c8555";
        assert_eq!(
            md5_from_etag(&format!("\"{hexed}\"")),
            Some(hex::decode(hexed).unwrap().try_into().unwrap())
        );
        assert_eq!(
            md5_from_etag(hexed).map(hex::encode).as_deref(),
            Some(hexed)
        );
        // A multipart ETag is a digest OF DIGESTS: reporting it as the content
        // md5 would make the executor's upload verification compare the wrong
        // thing and pass.
        assert_eq!(md5_from_etag(&format!("\"{hexed}-3\"")), None);
        assert_eq!(md5_from_etag("\"\""), None);
        assert_eq!(md5_from_etag("not-hex"), None);
    }

    #[test]
    fn composed_etag_matches_the_documented_s3_formula() {
        // AWS: the multipart ETag is hex(md5(concat(part md5 BYTES))) + "-N".
        let parts = vec![
            PartRecord {
                number: 1,
                md5: md5_of(b"aaa"),
                size: 3,
            },
            PartRecord {
                number: 2,
                md5: md5_of(b"bbb"),
                size: 3,
            },
        ];
        let mut concat = Vec::new();
        concat.extend_from_slice(&md5_of(b"aaa"));
        concat.extend_from_slice(&md5_of(b"bbb"));
        assert_eq!(
            composed_etag(&parts),
            format!("{}-2", hex::encode(md5_of(&concat)))
        );
    }

    #[test]
    fn session_urls_round_trip_keys_with_awkward_characters() {
        for (key, upload) in [
            ("a.txt", "u1"),
            ("nested/dir/a b+c%20.txt", "id/with+slashes=="),
            ("unicode/\u{00e9}\u{4e2d}.bin", "u2"),
        ] {
            let url = encode_session_url(key, upload);
            assert!(url.starts_with(SESSION_URL_SCHEME));
            assert_eq!(
                decode_session_url(&url).unwrap(),
                (key.into(), upload.into())
            );
        }
        // A session minted by another backend is rejected, not misread.
        assert!(decode_session_url("https://drive.example/upload/1").is_err());
        assert!(decode_session_url("driven-s3:!!!").is_err());
    }

    #[test]
    fn http_dates_parse_to_epoch_ms() {
        assert_eq!(parse_http_date_ms("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date_ms("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480_000)
        );
        // A leap day, to exercise the civil-from-days arithmetic.
        assert_eq!(
            parse_http_date_ms("Mon, 29 Feb 2016 00:00:00 GMT"),
            Some(1_456_704_000_000)
        );
        assert_eq!(parse_http_date_ms("nonsense"), None);
    }

    #[test]
    fn iso8601_instants_parse_to_epoch_ms() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso8601_ms("2010-11-10T20:48:33.000Z"),
            Some(1_289_422_113_000)
        );
        // No fractional part, and a leap day, are both shapes S3 emits.
        assert_eq!(
            parse_iso8601_ms("2016-02-29T00:00:00Z"),
            Some(1_456_704_000_000)
        );
        // Anything unrecognised is None, which makes the sweep SKIP the upload
        // rather than guess it is old enough to abort.
        assert_eq!(parse_iso8601_ms("yesterday"), None);
        assert_eq!(parse_iso8601_ms("2010-11-10"), None);
        assert_eq!(parse_iso8601_ms("2010-13-10T00:00:00Z"), None);
    }

    #[test]
    fn multipart_upload_listings_parse_keys_ids_and_ages() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListMultipartUploadsResult>
  <Bucket>bkt</Bucket>
  <NextKeyMarker>root/b.bin</NextKeyMarker>
  <NextUploadIdMarker>u2&amp;plus</NextUploadIdMarker>
  <IsTruncated>true</IsTruncated>
  <Upload>
    <Key>root/a.bin</Key>
    <UploadId>u1</UploadId>
    <Initiator><ID>x</ID></Initiator>
    <Initiated>2026-01-02T03:04:05.000Z</Initiated>
  </Upload>
  <Upload>
    <Key>root/b.bin</Key>
    <UploadId>u2&amp;plus</UploadId>
    <Initiated>2026-01-03T00:00:00.000Z</Initiated>
  </Upload>
</ListMultipartUploadsResult>"#;
        let page = parse_multipart_uploads(xml);
        assert!(page.truncated);
        assert_eq!(page.next_key_marker.as_deref(), Some("root/b.bin"));
        assert_eq!(page.next_upload_id_marker.as_deref(), Some("u2&plus"));
        assert_eq!(page.uploads.len(), 2);
        assert_eq!(page.uploads[0].key, "root/a.bin");
        assert_eq!(page.uploads[0].upload_id, "u1");
        assert_eq!(
            page.uploads[0].initiated_ms,
            parse_iso8601_ms("2026-01-02T03:04:05.000Z")
        );
        // `<Initiator>` must not be mistaken for `<Initiated>`, and an escaped
        // upload id must come back whole - an id truncated at the `&` would name
        // an upload the abort could never find.
        assert_eq!(page.uploads[1].upload_id, "u2&plus");
    }

    #[test]
    fn the_sweep_only_touches_an_upload_that_is_ours_unowned_and_long_dead() {
        let now = 30 * 24 * 60 * 60 * 1000; // day 30
        let long_ago = now - ABANDONED_UPLOAD_MIN_AGE_MS - 1;
        let upload = |key: &str, initiated: Option<i64>| InFlightUpload {
            key: key.to_string(),
            upload_id: "u1".to_string(),
            initiated_ms: initiated,
        };

        // The only case that gets aborted: under our prefix, not ours, ancient.
        assert!(is_sweepable(
            &upload("root/a.bin", Some(long_ago)),
            "root/",
            false,
            now
        ));

        // Someone else's bucket space, even if it is ancient. Driven shares
        // buckets with other tools and with other Driven destinations.
        assert!(!is_sweepable(
            &upload("other/a.bin", Some(long_ago)),
            "root/",
            false,
            now
        ));
        // An upload THIS process is in the middle of.
        assert!(!is_sweepable(
            &upload("root/a.bin", Some(long_ago)),
            "root/",
            true,
            now
        ));
        // Young enough that the executor could still resume it: the persisted
        // session window is 6 days and this threshold is 7, so anything inside
        // it is off limits.
        assert!(!is_sweepable(
            &upload("root/a.bin", Some(now - 1000)),
            "root/",
            false,
            now
        ));
        assert!(!is_sweepable(
            &upload("root/a.bin", Some(now - ABANDONED_UPLOAD_MIN_AGE_MS + 1)),
            "root/",
            false,
            now
        ));
        // Undateable: could be seconds old, so never touched. A leak that stays
        // visible beats a destroyed transfer.
        assert!(!is_sweepable(
            &upload("root/a.bin", None),
            "root/",
            false,
            now
        ));

        // At the bucket root the empty prefix matches everything Driven owns -
        // which is the whole bucket, by configuration.
        assert!(is_sweepable(
            &upload("a.bin", Some(long_ago)),
            "",
            false,
            now
        ));
    }

    #[test]
    fn an_upload_listing_with_no_uploads_is_a_complete_empty_answer() {
        let xml = "<ListMultipartUploadsResult><Bucket>bkt</Bucket>\
                   <IsTruncated>false</IsTruncated></ListMultipartUploadsResult>";
        let page = parse_multipart_uploads(xml);
        assert!(page.uploads.is_empty());
        assert!(!page.truncated);
        assert_eq!(page.next_key_marker, None);
        // An upload the server dated with something unparseable is kept in the
        // listing but carries no age, so the sweep leaves it alone.
        let undated = parse_multipart_uploads(
            "<R><Upload><Key>k</Key><UploadId>u</UploadId><Initiated>?</Initiated></Upload></R>",
        );
        assert_eq!(undated.uploads.len(), 1);
        assert_eq!(undated.uploads[0].initiated_ms, None);
    }

    #[test]
    fn a_copy_source_names_the_bucket_explicitly_in_both_addressing_styles() {
        // `x-amz-copy-source` is always `/<bucket>/<encoded key>`, whichever way
        // the endpoint is addressed - getting this wrong makes every archive
        // copy 404 against one provider and work against the other.
        let path_style = test_store(Some("root"));
        assert_eq!(
            path_style.copy_source_header("root/dir/a b.txt").unwrap(),
            "/bkt/root/dir/a%20b.txt"
        );

        let cfg = S3Config {
            endpoint: "https://s3.example.com".into(),
            bucket: "bkt".into(),
            region: "us-east-1".into(),
            path_style: false,
            prefix: Some("root".into()),
        }
        .normalized()
        .unwrap();
        let vhost = S3Store::new(
            &cfg,
            &S3Credentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
            },
            &CustomCaConfig::none(),
            &ProxyConfig::system(),
        )
        .unwrap();
        assert_eq!(
            vhost.copy_source_header("root/dir/a b.txt").unwrap(),
            "/bkt/root/dir/a%20b.txt"
        );
    }

    #[tokio::test]
    async fn trashing_an_archived_version_is_a_no_op_rather_than_a_delete() {
        // No HTTP mock is installed, so any request fails: reaching the
        // assertion is itself the proof that no DeleteObject was issued. On S3
        // `trash` is a permanent delete, so if the executor's Drive-shaped
        // "trash the superseded object" step reached the wire here it would
        // destroy the version it had just recorded (issue #220).
        let store = test_store(Some("root"));
        let archived = keys::version_key("root/", "root/docs/a.txt", "abc123");
        store.trash(&archived).await.unwrap();
        // A LIVE object still goes to the wire (and therefore fails here), which
        // is what proves the no-op is scoped to the version store and has not
        // quietly disabled deletion.
        assert!(store.trash("root/docs/a.txt").await.is_err());
    }

    #[tokio::test]
    async fn abandoning_a_session_from_another_backend_is_not_an_error() {
        // The executor hands over whatever it persisted; a session minted by a
        // different backend simply has nothing for this store to release, and
        // turning that into an error would make an abandoned-session cleanup
        // fail an otherwise healthy op.
        let store = test_store(None);
        let session = ResumableSession {
            url: "https://drive.example/upload/1".into(),
            issued_at: 0,
            size: 1,
            kind: ResumableKind::Update {
                file_id: "x".into(),
            },
        };
        store.abandon_resumable_session(&session).await.unwrap();
    }

    #[test]
    fn extract_tag_reads_the_first_match() {
        assert_eq!(
            extract_tag("<X><ETag>\"abc-2\"</ETag></X>", "ETag").as_deref(),
            Some("\"abc-2\"")
        );
        assert_eq!(extract_tag("<X/>", "ETag"), None);
    }

    #[test]
    fn extract_tag_unescapes_both_quote_encodings_servers_actually_send() {
        // Observed on real servers completing a multipart upload: MinIO emits
        // the NUMERIC entity and Cloudflare R2 the NAMED one, for the very same
        // quoted ETag. Comparing the raw text made every multipart upload look
        // like a checksum mismatch on both.
        let digest = "e7589679dcd23e0c7851429f24033a9e-3";
        for escaped in [
            format!("<R><ETag>&#34;{digest}&#34;</ETag></R>"),
            format!("<R><ETag>&quot;{digest}&quot;</ETag></R>"),
            format!("<R><ETag>\"{digest}\"</ETag></R>"),
        ] {
            let tag = extract_tag(&escaped, "ETag").expect("etag");
            assert_eq!(tag.trim_matches('"'), digest, "from {escaped}");
        }
    }

    #[test]
    fn xml_unescape_handles_the_predefined_entities_without_double_decoding() {
        assert_eq!(xml_unescape("plain"), "plain");
        assert_eq!(xml_unescape("&lt;a&gt; &apos;b&apos;"), "<a> 'b'");
        // A literal "&quot;" in the document is `&amp;quot;`; decoding `&amp;`
        // first would turn it into a quote.
        assert_eq!(xml_unescape("&amp;quot;"), "&quot;");
    }

    fn drain_state(buffer_len: usize, buffer_start: u64, parts: usize) -> MultipartState {
        let mut s = MultipartState::new();
        s.buffer = vec![0u8; buffer_len];
        s.buffer_start = buffer_start;
        s.parts = (0..parts)
            .map(|i| PartRecord {
                number: i as u16 + 1,
                md5: [0u8; 16],
                size: PART_SIZE as u64,
            })
            .collect();
        s
    }

    #[test]
    fn flushing_emits_full_parts_and_numbers_them_from_the_offset() {
        // Part numbers are a pure function of the offset, which is what lets a
        // replayed upload line its parts up with the ones already on the server.
        let mut s = drain_state(PART_SIZE * 2 + 7, 0, 0);
        assert_eq!(s.buffer_start, 0);
        let (n1, p1) = take_flushable(&mut s, false).expect("first full part");
        assert_eq!((n1, p1.len()), (1, PART_SIZE));
        s.buffer_start += PART_SIZE as u64;
        assert_eq!(s.buffer_start, PART_SIZE as u64);
        let (n2, p2) = take_flushable(&mut s, false).expect("second full part");
        assert_eq!((n2, p2.len()), (2, PART_SIZE));
        s.buffer_start += PART_SIZE as u64;
        // A short tail is NOT flushed mid-body (S3 rejects a small non-final
        // part), only once the body is complete.
        assert!(take_flushable(&mut s, false).is_none());
        let (n3, p3) = take_flushable(&mut s, true).expect("short final part");
        assert_eq!((n3, p3.len()), (3, 7));
    }

    #[test]
    fn a_zero_byte_body_still_emits_one_part() {
        // CompleteMultipartUpload rejects an empty part list, so a body that
        // produced nothing must still flush a single empty part.
        let mut s = drain_state(0, 0, 0);
        assert!(take_flushable(&mut s, false).is_none());
        let (n, p) = take_flushable(&mut s, true).expect("one empty part");
        assert_eq!((n, p.len()), (1, 0));
    }

    #[test]
    fn a_fresh_session_accepts_a_chunk_at_its_consumed_offset_and_refuses_gaps() {
        let mut s = MultipartState::new();
        // A session this process opened starts at 0 and needs no rewind.
        assert_eq!(seek_verdict(&mut s, 0), None);
        // Pretend a part flushed and 1 KiB is buffered.
        s.buffer_start = PART_SIZE as u64;
        s.buffer = vec![0u8; 1024];
        let consumed = PART_SIZE as u64 + 1024;
        assert_eq!(seek_verdict(&mut s, consumed), None);
        // A GAP would leave a hole in the object; an OVERLAP would duplicate
        // bytes. Both are answered with the true offset rather than accepted.
        assert_eq!(seek_verdict(&mut s, consumed + 4096), Some(consumed));
        assert_eq!(seek_verdict(&mut s, consumed - 512), Some(consumed));
    }

    #[test]
    fn a_hydrated_session_rewinds_to_zero_exactly_once() {
        // The persisted offset counts bytes the store ACCEPTED, including any
        // that were still buffered when the process died. Resuming there would
        // drop them out of the middle of the object, so the first chunk after a
        // restart is answered with a rewind to 0.
        let mut s = MultipartState::new();
        s.rewound = false;
        assert_eq!(
            seek_verdict(&mut s, 40 * 1024 * 1024),
            Some(0),
            "a hydrated session must replay from the start"
        );
        // Bounded to once: after the rewind the normal offset rules apply, so a
        // caller that ignored the rewind gets a definite answer instead of a
        // loop.
        assert_eq!(seek_verdict(&mut s, 40 * 1024 * 1024), Some(0));
        assert_eq!(seek_verdict(&mut s, 0), None);
    }

    #[test]
    fn a_hydrated_session_already_at_zero_needs_no_rewind() {
        let mut s = MultipartState::new();
        s.rewound = false;
        assert_eq!(seek_verdict(&mut s, 0), None);
    }

    #[test]
    fn a_replayed_part_skips_the_upload_only_on_an_exact_size_and_digest_match() {
        let bytes = b"the part payload";
        let md5 = md5_of(bytes);
        let size = bytes.len() as u64;
        let etag = hex::encode(md5);

        assert!(part_already_uploaded(
            Some(&(etag.clone(), size)),
            &md5,
            size
        ));
        // Quoted ETags (what a server actually returns) must match too.
        assert!(part_already_uploaded(
            Some(&(format!("\"{etag}\""), size)),
            &md5,
            size
        ));

        // A matching digest with a DIFFERENT size must not skip: accepting it
        // would let a truncated part masquerade as complete.
        assert!(!part_already_uploaded(
            Some(&(etag.clone(), size + 1)),
            &md5,
            size
        ));
        // A different digest never skips.
        assert!(!part_already_uploaded(
            Some(&(hex::encode(md5_of(b"other")), size)),
            &md5,
            size
        ));
        // A part the previous run never uploaded is not on the server.
        assert!(!part_already_uploaded(None, &md5, size));
        // A garbage ETag is not a match.
        assert!(!part_already_uploaded(
            Some(&("not-a-digest".to_string(), size)),
            &md5,
            size
        ));
    }

    #[test]
    fn a_completed_body_with_an_empty_buffer_flushes_nothing_more() {
        let mut s = drain_state(0, PART_SIZE as u64, 1);
        assert!(take_flushable(&mut s, true).is_none());
    }

    #[test]
    fn session_fatality_keeps_transient_failures_retryable() {
        use driven_remote::remote_store::DriveErrorClassification as C;
        for kind in [
            C::Transient5xx,
            C::Network,
            C::RateLimited { retry_after_ms: 1 },
        ] {
            let e = anyhow::Error::new(DriveError::Classified {
                kind,
                source: anyhow::anyhow!("x"),
            });
            assert!(!is_session_fatal(&e));
        }
        for kind in [C::AuthInvalidGrant, C::Other, C::StorageQuota] {
            let e = anyhow::Error::new(DriveError::Classified {
                kind,
                source: anyhow::anyhow!("x"),
            });
            assert!(is_session_fatal(&e));
        }
        assert!(is_session_fatal(&anyhow::Error::new(
            DriveError::ChecksumMismatch {
                stranded_file_id: None
            }
        )));
    }

    fn test_store(prefix: Option<&str>) -> S3Store {
        let cfg = S3Config {
            endpoint: "https://s3.example.com".into(),
            bucket: "bkt".into(),
            region: "us-east-1".into(),
            path_style: true,
            prefix: prefix.map(str::to_string),
        }
        .normalized()
        .unwrap();
        S3Store::new(
            &cfg,
            &S3Credentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
            },
            &CustomCaConfig::none(),
            &ProxyConfig::system(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn ensure_folder_synthesizes_a_prefix_and_issues_no_request() {
        // No HTTP mock is installed, so any request would fail: reaching the
        // assertion proves this path is offline.
        let store = test_store(Some("root"));
        assert_eq!(store.root_id(), "root/");
        let a = store
            .ensure_folder("root/", "docs", &DriveContext::MyDrive)
            .await
            .unwrap();
        assert_eq!(a.id, "root/docs/");
        assert_eq!(a.name, "docs");
        assert_eq!(a.parents, vec!["root/".to_string()]);
        assert!(a.size.is_none());
        assert!(!a.trashed);

        let b = store
            .ensure_folder(&a.id, "sub", &DriveContext::MyDrive)
            .await
            .unwrap();
        assert_eq!(b.id, "root/docs/sub/");
        // Idempotent by construction: the same inputs give the same id.
        let again = store
            .ensure_folder("root/", "docs", &DriveContext::MyDrive)
            .await
            .unwrap();
        assert_eq!(again.id, a.id);
    }

    #[test]
    fn signed_urls_use_the_configured_addressing_style_and_region() {
        let path_style = test_store(None);
        let url = actions::PutObject::new(
            &path_style.bucket,
            Some(&path_style.credentials),
            "dir/obj.bin",
        )
        .sign(SIGN_EXPIRY);
        assert_eq!(url.host_str(), Some("s3.example.com"));
        assert!(
            url.path().starts_with("/bkt/"),
            "path style puts the bucket in the path: {url}"
        );
        let query = url.query().unwrap_or_default();
        assert!(query.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(
            query.contains("us-east-1%2Fs3%2Faws4_request"),
            "the signing region must appear in the credential scope: {query}"
        );

        let cfg = S3Config {
            endpoint: "https://s3.example.com".into(),
            bucket: "bkt".into(),
            region: "us-east-1".into(),
            path_style: false,
            prefix: None,
        }
        .normalized()
        .unwrap();
        let vhost = S3Store::new(
            &cfg,
            &S3Credentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
            },
            &CustomCaConfig::none(),
            &ProxyConfig::system(),
        )
        .unwrap();
        let url = actions::PutObject::new(&vhost.bucket, Some(&vhost.credentials), "dir/obj.bin")
            .sign(SIGN_EXPIRY);
        assert_eq!(url.host_str(), Some("bkt.s3.example.com"));
        assert!(url.path().starts_with("/dir/"));
    }

    #[test]
    fn content_md5_is_part_of_the_signed_header_set() {
        // The integrity story depends on Content-MD5 actually reaching the
        // server AND being covered by the signature: if the signer dropped it,
        // a proxy could strip the header and S3 would stop verifying our bytes.
        let store = test_store(None);
        let mut action =
            actions::PutObject::new(&store.bucket, Some(&store.credentials), "obj.bin");
        action
            .headers_mut()
            .insert("content-md5", "rL0Y20zC+Fzt72VPzMSk2A==".to_string());
        let query = action
            .sign(SIGN_EXPIRY)
            .query()
            .unwrap_or_default()
            .to_string();
        assert!(
            query.contains("X-Amz-SignedHeaders=content-md5%3Bhost"),
            "content-md5 must be signed: {query}"
        );
    }

    #[test]
    fn metadata_headers_carry_props_and_the_content_digest() {
        let mut props = HashMap::new();
        props.insert(
            driven_remote::props::SOURCE_ID_KEY.to_string(),
            "src-1".to_string(),
        );
        let md5 = md5_of(b"hello");
        let headers = S3Store::metadata_headers(&props, Some(&md5)).unwrap();
        let map: HashMap<_, _> = headers.into_iter().collect();
        assert_eq!(
            map.get("x-amz-meta-driven-md5").map(String::as_str),
            Some(hex::encode(md5).as_str())
        );
        let encoded = map.get("x-amz-meta-driven-props").expect("props header");
        assert_eq!(keys::decode_props(Some(encoded)), props);

        // No properties and no digest means no metadata headers at all.
        assert!(S3Store::metadata_headers(&HashMap::new(), None)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn collect_stream_rejects_a_length_mismatch_in_both_directions() {
        let over = Box::new(futures::stream::iter(vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ]));
        assert!(S3Store::collect_stream(4, over).await.is_err());

        let under = Box::new(futures::stream::iter(vec![Ok(Bytes::from_static(b"ab"))]));
        assert!(S3Store::collect_stream(5, under).await.is_err());

        let exact = Box::new(futures::stream::iter(vec![Ok(Bytes::from_static(
            b"abcde",
        ))]));
        assert_eq!(
            S3Store::collect_stream(5, exact).await.unwrap(),
            Bytes::from_static(b"abcde")
        );
    }
}
