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
    /// Total-capped client for metadata / control requests.
    http: reqwest::Client,
    /// Idle-timeout-only client for body transfers.
    http_stream: reqwest::Client,
    /// In-flight multipart uploads, keyed by upload id.
    uploads: Mutex<HashMap<String, MultipartState>>,
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
            http: build_meta_client(ca, proxy)?,
            http_stream: build_stream_client(ca, proxy)?,
            uploads: Mutex::new(HashMap::new()),
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
        Ok(parsed.upload_id().to_string())
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
        let action = actions::AbortMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            upload_id,
        );
        let url = action.sign(SIGN_EXPIRY);
        if let Err(err) = self
            .execute(&self.http, reqwest::Method::DELETE, url, &[], None)
            .await
        {
            tracing::warn!(
                target: crate::TARGET,
                %err,
                "failed to abort a multipart upload; its parts may linger until a bucket lifecycle rule reaps them"
            );
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

    // Days since the Unix epoch, via the civil-from-days algorithm (Howard
    // Hinnant's `days_from_civil`), which is exact for the whole proleptic
    // Gregorian calendar.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3_600 + min * 60 + sec) * 1_000)
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
        // replayed yet.
        {
            let mut uploads = self.uploads.lock();
            let state = uploads
                .get_mut(&upload_id)
                .ok_or_else(|| anyhow::anyhow!("s3.session_invalid: upload state vanished"))?;
            if !state.rewound {
                state.rewound = true;
                if offset != 0 {
                    return Ok(ResumeProgress::InProgress { received: 0 });
                }
            }
            if offset != state.consumed() {
                // The caller is not where we are. Refuse rather than write the
                // wrong bytes into the middle of a part.
                let received = state.consumed();
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
    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
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
    async fn list_source_object_ids(
        &self,
        source_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<HashSet<String>> {
        let mut all_keys = Vec::new();
        self.list_pages(&self.prefix, None, |page| {
            for obj in &page.contents {
                // Skip any zero-byte directory marker another tool left behind.
                if !obj.key.ends_with('/') {
                    all_keys.push(obj.key.clone());
                }
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
        if let Some((etag, existing_size)) = already {
            if existing_size == size && parse_md5_hex(&etag) == Some(md5) {
                tracing::debug!(
                    target: crate::TARGET,
                    number,
                    "part already on the server with a matching digest; skipping the upload"
                );
                return Ok(PartRecord { number, md5, size });
            }
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
