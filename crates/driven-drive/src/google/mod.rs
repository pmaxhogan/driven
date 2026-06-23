//! The production Google Drive [`RemoteStore`] backend (SPEC s3, s4, s8;
//! ROADMAP M4).
//!
//! [`GoogleDriveStore`] talks to the Drive v3 REST API over a `reqwest`
//! (rustls) client, authorized by a [`RefreshingTokenSource`] that mints
//! access tokens from the keychain-stored refresh token (SPEC s4.1). It
//! honours the Drive semantics the trait requires literally: duplicate names
//! are allowed (every lookup is by file_id), `create` is always POST and
//! `update` is always PATCH, resumable non-final chunks are a multiple of
//! 256 KiB, and `app_properties` is the canonical identity for objects
//! Driven owns.
//!
//! The submodules carry the protocol helpers [`GoogleDriveStore`] composes:
//! - [`oauth`] - the PKCE loopback flow (SPEC s4).
//! - [`token_store`] - keychain-backed refresh-token storage +
//!   [`RefreshingTokenSource`] (SPEC s4.1).
//! - [`resumable`] - the resumable-upload session protocol.
//! - [`pagination`] - the `files.list` `pageToken` loop.
//! - [`retry`] - exponential-backoff + error-classification middleware.
//!
//! ## Drive endpoints used (M4)
//!
//! | trait method        | Drive endpoint                                                            |
//! |---------------------|---------------------------------------------------------------------------|
//! | `ensure_folder`     | `GET /drive/v3/files` (search) + `POST /drive/v3/files` (create folder)    |
//! | `list_folder`       | `GET /drive/v3/files` (paginated)                                         |
//! | `create` (small)    | `POST /upload/drive/v3/files?uploadType=multipart`                         |
//! | `create` (stream)   | resumable session `POST` + chunk `PUT` (see `resumable`)                   |
//! | `update` (small)    | `PATCH /upload/drive/v3/files/{id}?uploadType=multipart`                   |
//! | `update` (stream)   | resumable session `PATCH` + chunk `PUT` (see `resumable`)                  |
//! | `resumable_session` | `POST` / `PATCH /upload/drive/v3/files[/{id}]?uploadType=resumable`        |
//! | `resume_chunk`      | `PUT <session-url>` with `Content-Range`                                  |
//! | `trash`             | `PATCH /drive/v3/files/{id}` `{trashed:true}`                              |
//! | `metadata`          | `GET /drive/v3/files/{id}?fields=..`                                       |
//! | `download`          | `GET /drive/v3/files/{id}?alt=media`                                       |
//! | `find_by_op_uuid`   | `GET /drive/v3/files` (`appProperties has {..}` query)                     |
//! | `about`             | `GET /drive/v3/about?fields=storageQuota`                                  |

pub mod oauth;
pub mod pagination;
pub mod resumable;
pub mod retry;
pub mod token_store;

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::Deserialize;
use tokio::io::{AsyncRead, ReadBuf};
use tracing::warn;

use crate::remote_store::{
    AboutInfo, DownloadStream, DriveErrorClassification, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, UploadBody,
};

use self::token_store::RefreshingTokenSource;

/// Tracing target for the Google Drive backend.
const TARGET: &str = "driven::drive::google";

/// Re-export of [`bytes::Bytes`] so downstream crates (e.g. `driven-cli`) can
/// build an [`UploadBody::Bytes`] without declaring `bytes` themselves.
pub use bytes::Bytes as UploadBytes;

/// Hex-encodes a 16-byte md5 digest (helper for CLI display so callers need no
/// `hex` dep).
pub fn md5_hex(md5: &[u8; 16]) -> String {
    hex::encode(md5)
}

/// The Google "installed app" client config JSON shape (the console download:
/// `{"installed": {"client_id":..,"client_secret":..}}`). Re-exposed so the
/// CLI can read `client_secret.json` without its own `serde` dep.
#[derive(Debug, Deserialize)]
pub struct InstalledClientConfig {
    /// The `installed` block carrying the client credentials.
    pub installed: InstalledClient,
}

/// The credential fields inside an [`InstalledClientConfig`].
#[derive(Debug, Deserialize)]
pub struct InstalledClient {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: String,
}

/// Parses an installed-app client config JSON (`client_secret.json`) into its
/// `(client_id, client_secret)`. The CLI calls this so it needs no `serde`
/// dependency of its own.
pub fn parse_installed_client_config(bytes: &[u8]) -> anyhow::Result<(String, String)> {
    let config: InstalledClientConfig = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("failed to parse installed-app client config: {e}"))?;
    Ok((config.installed.client_id, config.installed.client_secret))
}

/// Drive v3 REST API base (metadata operations).
pub(crate) const DRIVE_API_BASE: &str = "https://www.googleapis.com/drive/v3";

/// Drive v3 upload base (multipart + resumable content operations).
pub(crate) const DRIVE_UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";

/// MIME type Drive uses for folders.
pub(crate) const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// `app_properties` key marking folders Driven created (SPEC s3
/// `ensure_folder` disambiguation).
pub(crate) const FOLDER_MARKER_KEY: &str = "driven.folder_marker";

/// `app_properties` key carrying the crash-safe create-op UUID (DESIGN s5.6).
pub(crate) const CLIENT_OP_UUID_KEY: &str = "driven.client_op_uuid";

/// Files at or above this go through the resumable upload protocol; below
/// uses a simple multipart upload (DESIGN s5.4 `RESUMABLE_THRESHOLD = 5 MiB`).
pub(crate) const RESUMABLE_THRESHOLD: u64 = 5 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Per-operation HTTP timeouts (DESIGN s5.8.4). reqwest's per-call API does not
// let us vary connect/idle granularly, so we build a small client per timeout
// profile once at construction and route each call to the right one.
// ---------------------------------------------------------------------------

/// Connect timeout shared by all Drive profiles (DESIGN s5.8.4).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total-request timeout for Drive metadata calls (about / list / get / patch).
const META_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Total-request timeout for a Drive simple (<=5 MiB) multipart upload.
const SIMPLE_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Total-request timeout for opening / committing a resumable session.
const RESUMABLE_CTRL_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle (between-bytes) read timeout for a resumable chunk PUT / download:
/// no overall cap, but a stuck transfer is caught by this idle timeout
/// (DESIGN s5.8.4 `*`).
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A classified Google Drive error (SPEC s24 error taxonomy).
///
/// Carries a [`DriveErrorClassification`] (re-used from
/// [`crate::remote_store`]) so the executor / pacer / circuit-breakers can
/// decide breaker outcomes by downcasting an [`anyhow::Error`] back to this
/// type rather than string-matching the message (CODEX_NOTES: "Drive circuit
/// breaker driven by real request outcomes"). Surfaced through `anyhow` at
/// the trait boundary; recover the classification with
/// [`classification_of`].
///
/// The `Display` text deliberately EMBEDS the SPEC s24 dotted error code as a
/// literal substring (`drive.rate_limited`, `auth.invalid_grant`, ...) so the
/// M3 executor's `classify_drive_error`, which still classifies by
/// case-sensitive substring on `e.to_string()` until agent P switches it to
/// the [`classification_of`] downcast, classifies a real-store error the same
/// way it classifies the fake's messages. Both paths therefore agree.
///
/// `Display`/`Error` are hand-written (not `thiserror`-derived) so the
/// `Classified` message can match on its `kind` field directly - emitting the
/// right SPEC s24 dotted code - without relying on a function-call-in-attribute
/// expansion.
#[derive(Debug)]
pub enum DriveError {
    /// A classified API/transport failure (429 / 5xx / network / auth /
    /// quota / other). The variant payload IS the pacer/breaker verdict.
    Classified {
        /// How the pacer + circuit breaker should treat this failure.
        kind: DriveErrorClassification,
        /// The underlying cause (HTTP status, transport error, parse error).
        source: anyhow::Error,
    },
    /// The configured destination folder was deleted from Drive (SPEC s24
    /// `drive.dest_folder_missing`).
    DestFolderMissing,
    /// The destination folder's sharing changed to read-only for this
    /// account (SPEC s24 `drive.dest_folder_permission_denied`).
    DestFolderPermissionDenied,
    /// A resumable upload session returned a 4xx mid-chunk; the caller must
    /// restart from offset 0 (SPEC s24 `drive.resumable_session_invalid`).
    ResumableSessionInvalid,
    /// Verification of the uploaded bytes failed: Drive's `md5Checksum` did
    /// not match the bytes Driven sent (SPEC s24 `drive.checksum_mismatch`).
    ChecksumMismatch,
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::Classified { kind, .. } => write!(f, "{}", classified_message(kind)),
            DriveError::DestFolderMissing => {
                write!(f, "drive.dest_folder_missing: destination folder is missing")
            }
            DriveError::DestFolderPermissionDenied => write!(
                f,
                "drive.dest_folder_permission_denied: destination folder is read-only for this account"
            ),
            DriveError::ResumableSessionInvalid => write!(
                f,
                "drive.resumable_session_invalid: resumable session is invalid; restart required"
            ),
            DriveError::ChecksumMismatch => {
                write!(f, "drive.checksum_mismatch: md5 mismatch after upload")
            }
        }
    }
}

impl std::error::Error for DriveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // The `anyhow::Error` source is surfaced as the error chain so the
            // `is_not_found` helper (and any caller) can walk to the
            // `drive HTTP <status>` cause. `anyhow::Error` impls
            // `AsRef<dyn Error + Send + Sync>`; coerce off the auto traits to
            // the `source()` return type.
            DriveError::Classified { source, .. } => {
                let std_err: &(dyn std::error::Error + Send + Sync + 'static) = source.as_ref();
                Some(std_err)
            }
            _ => None,
        }
    }
}

/// Builds the `Display` text for a [`DriveError::Classified`], embedding the
/// SPEC s24 dotted code so BOTH the downcast path ([`classification_of`]) and
/// the M3 string-substring matcher (`executor.rs::classify_drive_error`)
/// agree on the class. The matcher tests `daily` before `quota_exhausted`, so
/// the daily-quota code must contain `daily` (it does:
/// `drive.daily_quota_exhausted`).
fn classified_message(kind: &DriveErrorClassification) -> String {
    match kind {
        DriveErrorClassification::RateLimited { retry_after_ms } => {
            format!("drive.rate_limited (retry_after_ms={retry_after_ms})")
        }
        DriveErrorClassification::Transient5xx => {
            "drive.unreachable: transient 5xx from Drive".to_string()
        }
        DriveErrorClassification::Network => {
            "net.intermittent: drive request network/transport error".to_string()
        }
        DriveErrorClassification::AuthInvalidGrant => {
            "auth.invalid_grant: refresh token revoked; reauth required".to_string()
        }
        DriveErrorClassification::DailyQuota => {
            "drive.daily_quota_exhausted: 403 dailyLimitExceeded".to_string()
        }
        DriveErrorClassification::StorageQuota => {
            "drive.quota_exhausted: 403 storageQuotaExceeded".to_string()
        }
        DriveErrorClassification::Other => {
            "drive.unreachable: unclassified Drive error".to_string()
        }
    }
}

impl DriveError {
    /// The [`DriveErrorClassification`] this error implies, for the pacer +
    /// circuit breaker. Non-[`DriveError::Classified`] variants map to their
    /// natural class ([`DriveErrorClassification::Other`] for the fatal
    /// dest-folder / checksum / session-invalid cases).
    pub fn classification(&self) -> DriveErrorClassification {
        match self {
            DriveError::Classified { kind, .. } => kind.clone(),
            DriveError::DestFolderMissing
            | DriveError::DestFolderPermissionDenied
            | DriveError::ResumableSessionInvalid
            | DriveError::ChecksumMismatch => DriveErrorClassification::Other,
        }
    }

    /// Builds a classified error from a Drive HTTP status + body, mapping the
    /// status/reason to the SPEC s24 class via [`retry::classify_response`].
    /// The dest-folder-missing / permission-denied 404/403 cases against the
    /// destination folder are promoted to their dedicated fatal variants by
    /// the caller (which knows it was a write against the dest folder).
    pub(crate) fn from_response(status: u16, body: &[u8], retry_after_ms: Option<u64>) -> Self {
        let kind = retry::classify_response(status, body, retry_after_ms);
        DriveError::Classified {
            kind,
            source: anyhow::anyhow!(
                "drive HTTP {status}: {}",
                String::from_utf8_lossy(body)
                    .chars()
                    .take(512)
                    .collect::<String>()
            ),
        }
    }

    /// Builds a classified transport-error from a `reqwest::Error`.
    pub(crate) fn from_transport(err: reqwest::Error) -> Self {
        let kind = retry::classify_transport_error(&err);
        DriveError::Classified {
            kind,
            source: anyhow::Error::new(err),
        }
    }
}

/// Reads the [`DriveErrorClassification`] off an [`anyhow::Error`] the trait
/// boundary surfaced, if it originated as a [`DriveError`] (the executor
/// downcasts to decide breaker outcomes; CODEX_NOTES "Drive circuit breaker
/// driven by real request outcomes"). Returns `None` for any other error.
pub fn classification_of(err: &anyhow::Error) -> Option<DriveErrorClassification> {
    err.downcast_ref::<DriveError>()
        .map(DriveError::classification)
}

// ---------------------------------------------------------------------------
// Drive JSON shapes.
// ---------------------------------------------------------------------------

/// The Drive `files` resource shape Driven reads back (the [`FILE_FIELDS`]
/// projection; [`pagination::FILE_FIELDS`]).
#[derive(Debug, Deserialize)]
pub(crate) struct DriveFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parents: Vec<String>,
    /// Drive returns size as a STRING (`"1234"`) for the v3 API.
    #[serde(default)]
    pub size: Option<String>,
    #[serde(rename = "md5Checksum", default)]
    pub md5_checksum: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
    /// RFC 3339 timestamp string.
    #[serde(rename = "modifiedTime", default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub trashed: bool,
    #[serde(rename = "appProperties", default)]
    pub app_properties: HashMap<String, String>,
}

impl DriveFile {
    /// Converts a parsed Drive resource into a [`RemoteEntry`].
    pub(crate) fn into_remote_entry(self) -> RemoteEntry {
        let size = self.size.as_deref().and_then(|s| s.parse::<u64>().ok());
        let md5 = self.md5_checksum.as_deref().and_then(parse_md5_hex);
        let modified_time = self
            .modified_time
            .as_deref()
            .and_then(parse_rfc3339_to_unix_ms)
            .unwrap_or(0);
        RemoteEntry {
            id: self.id,
            name: self.name,
            parents: self.parents,
            size,
            md5,
            mime_type: self.mime_type,
            modified_time,
            trashed: self.trashed,
            app_properties: self.app_properties,
        }
    }
}

/// Parses Drive's `about` `storageQuota` shape into an [`AboutInfo`].
#[derive(Debug, Deserialize)]
struct AboutResponse {
    #[serde(rename = "storageQuota")]
    storage_quota: StorageQuota,
}

#[derive(Debug, Deserialize)]
struct StorageQuota {
    /// `None`/absent for unlimited (Workspace).
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    usage: Option<String>,
    #[serde(rename = "usageInDrive", default)]
    usage_in_drive: Option<String>,
    #[serde(rename = "usageInDriveTrash", default)]
    usage_in_drive_trash: Option<String>,
}

/// Parses a 32-hex-char md5 string into 16 bytes; `None` if malformed.
pub(crate) fn parse_md5_hex(s: &str) -> Option<[u8; 16]> {
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 16 {
        return None;
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Parses an RFC 3339 timestamp (`2024-01-02T03:04:05.678Z`) into Unix epoch
/// ms. Hand-rolled (no chrono dep) for the small set of shapes Drive emits:
/// `YYYY-MM-DDTHH:MM:SS[.fff]Z`.
pub(crate) fn parse_rfc3339_to_unix_ms(s: &str) -> Option<i64> {
    // Split date and time on 'T'.
    let (date, rest) = s.split_once('T')?;
    let time = rest.trim_end_matches('Z');
    let (time, frac) = match time.split_once('.') {
        Some((t, f)) => (t, Some(f)),
        None => (time, None),
    };
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next()?.parse().ok()?;
    let millis: i64 = match frac {
        Some(f) => {
            // Take the first 3 digits, zero-pad to ms.
            let mut ms = 0i64;
            for (i, c) in f.chars().take(3).enumerate() {
                let digit = c.to_digit(10)? as i64;
                ms += digit * 10i64.pow(2 - i as u32);
            }
            ms
        }
        None => 0,
    };
    // Days since Unix epoch (1970-01-01) via the civil-from-days algorithm
    // (Howard Hinnant's date algorithms), valid for the full Gregorian range.
    let days = days_from_civil(year, month, day);
    let total_secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(total_secs * 1000 + millis)
}

/// Days since 1970-01-01 for a civil (proleptic Gregorian) date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// GoogleDriveStore.
// ---------------------------------------------------------------------------

/// The production Google Drive [`RemoteStore`] (SPEC s3, ROADMAP M4).
///
/// Holds the authorized HTTP clients (a `reqwest` rustls client per
/// timeout profile, DESIGN s5.8.4) and the [`RefreshingTokenSource`]
/// (SPEC s4.1). Cheap to clone-by-`Arc` internally (the `reqwest::Client`
/// and the token source are both `Arc`-backed); the orchestrator holds it
/// behind `Arc<dyn RemoteStore>`.
pub struct GoogleDriveStore {
    /// Client for metadata calls (about / list / get / patch / multipart
    /// create) with the DESIGN s5.8.4 metadata timeouts. The default
    /// `meta` profile carries the 30s total cap; the upload paths reuse
    /// this for the small multipart bodies (<= 5 MiB) since the 60s cap
    /// only matters under genuine loss, and `with_retry` re-attempts.
    http: reqwest::Client,
    /// Client for the resumable chunk PUT + download streams: no overall
    /// timeout (chunks/downloads can be arbitrarily large), only the idle
    /// between-bytes timeout (DESIGN s5.8.4 `*`).
    http_stream: reqwest::Client,
    tokens: RefreshingTokenSource,
}

impl GoogleDriveStore {
    /// Builds a [`GoogleDriveStore`] from an authorized HTTP client and a
    /// [`RefreshingTokenSource`] (SPEC s4.1). The token source mints access
    /// tokens on demand from the keychain-stored refresh token; `http` is
    /// the `reqwest` (rustls) client metadata/short-upload Drive traffic
    /// flows over (the caller builds it with the DESIGN s5.8.4 metadata
    /// timeouts). A second, no-overall-timeout streaming client is derived
    /// for the resumable chunk PUT + download paths.
    pub fn new(http: reqwest::Client, tokens: RefreshingTokenSource) -> Self {
        let _ = TARGET;
        // The streaming client mirrors `http`'s TLS/proxy config but drops
        // the overall request cap; if the dedicated build fails we fall back
        // to the provided client (correctness over a missing idle timeout).
        let http_stream = build_stream_client().unwrap_or_else(|e| {
            warn!(
                target: TARGET,
                error = %e,
                "failed to build streaming Drive client; reusing the metadata client (no idle-only timeout)"
            );
            http.clone()
        });
        Self {
            http,
            http_stream,
            tokens,
        }
    }

    /// Builds a [`GoogleDriveStore`] with internally-constructed Drive HTTP
    /// clients (DESIGN s5.8.4 timeouts) from a [`RefreshingTokenSource`].
    /// Convenience for the CLI / e2e paths that do not already hold a tuned
    /// client.
    pub fn with_default_clients(tokens: RefreshingTokenSource) -> anyhow::Result<Self> {
        let http = build_meta_client()?;
        let http_stream = build_stream_client()?;
        Ok(Self {
            http,
            http_stream,
            tokens,
        })
    }

    /// Mints a fresh bearer token for an authorized request (SPEC s4.1).
    pub(crate) async fn bearer(&self) -> anyhow::Result<String> {
        self.tokens.access_token().await
    }

    /// Reference to the streaming HTTP client (resumable chunk + download).
    pub(crate) fn http_stream(&self) -> &reqwest::Client {
        &self.http_stream
    }

    /// Sends a metadata request and returns the parsed JSON body of type `R`,
    /// wrapping it in [`with_retry`](retry::with_retry) so the transient
    /// classes are retried. `build` re-builds the `RequestBuilder` per attempt
    /// (a fresh bearer + fresh body each time).
    async fn send_json<R, B>(&self, build: B) -> anyhow::Result<R>
    where
        R: serde::de::DeserializeOwned,
        B: Fn(String) -> reqwest::RequestBuilder,
    {
        let body = retry::with_retry(|| async {
            let token = self.bearer().await?;
            let resp = build(token)
                .send()
                .await
                .map_err(DriveError::from_transport)?;
            let status = resp.status().as_u16();
            let retry_after = parse_retry_after(&resp);
            let bytes = resp.bytes().await.map_err(DriveError::from_transport)?;
            if (200..300).contains(&status) {
                Ok(bytes)
            } else {
                Err(anyhow::Error::new(DriveError::from_response(
                    status,
                    &bytes,
                    retry_after,
                )))
            }
        })
        .await?;
        serde_json::from_slice::<R>(&body)
            .map_err(|e| anyhow::anyhow!("drive: failed to parse response JSON: {e}"))
    }

    /// Searches `parent_id`'s non-trashed children matching a Drive query and
    /// returns the parsed entries (used by `ensure_folder` /
    /// `find_by_op_uuid`).
    pub(crate) async fn list_query(&self, q: &str) -> anyhow::Result<Vec<RemoteEntry>> {
        let token = self.bearer().await?;
        pagination::list_all(&self.http, &token, q).await
    }

    /// Creates a folder under `parent_id` with the Driven folder marker
    /// (SPEC s3 `ensure_folder` create path).
    async fn create_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry> {
        let mut app_properties = HashMap::new();
        app_properties.insert(FOLDER_MARKER_KEY.to_string(), "1".to_string());
        let body = json_body(&serde_json::json!({
            "name": name,
            "mimeType": FOLDER_MIME,
            "parents": [parent_id],
            "appProperties": app_properties,
        }))?;
        let file: DriveFile = self
            .send_json(|token| {
                self.http
                    .post(format!("{DRIVE_API_BASE}/files"))
                    .query(&[("fields", pagination::FILE_FIELDS)])
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;
        Ok(file.into_remote_entry())
    }

    /// Multipart create/update for a small (`<= RESUMABLE_THRESHOLD`) body
    /// (DESIGN s5.4). `file_id` is `Some` for an update (PATCH by id) and
    /// `None` for a create (POST). md5-verifies the result (SPEC s8).
    async fn multipart_upload(
        &self,
        file_id: Option<&str>,
        parent_id: Option<&str>,
        name: Option<&str>,
        mime: &str,
        content: Bytes,
        app_properties: &HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let expected_md5 = md5_of(&content);

        // Drive multipart: a `multipart/related` body whose first part is the
        // JSON metadata and second part is the raw content.
        let mut metadata = serde_json::Map::new();
        if let Some(n) = name {
            metadata.insert("name".to_string(), serde_json::json!(n));
        }
        if let Some(p) = parent_id {
            metadata.insert("parents".to_string(), serde_json::json!([p]));
        }
        if !app_properties.is_empty() {
            metadata.insert(
                "appProperties".to_string(),
                serde_json::json!(app_properties),
            );
        }
        let metadata_json = serde_json::to_vec(&serde_json::Value::Object(metadata))?;

        let boundary = format!("driven-{}", uuid_v4());
        let body = build_multipart_related(&boundary, &metadata_json, mime, &content);

        let url = match file_id {
            Some(id) => format!("{DRIVE_UPLOAD_BASE}/files/{id}"),
            None => format!("{DRIVE_UPLOAD_BASE}/files"),
        };
        let content_type = format!("multipart/related; boundary={boundary}");

        let file: DriveFile = self
            .send_json(|token| {
                self.http
                    .request(
                        if file_id.is_some() {
                            reqwest::Method::PATCH
                        } else {
                            reqwest::Method::POST
                        },
                        &url,
                    )
                    .query(&[
                        ("uploadType", "multipart"),
                        ("fields", pagination::FILE_FIELDS),
                    ])
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, &content_type)
                    .timeout(SIMPLE_UPLOAD_TOTAL_TIMEOUT)
                    .body(body.clone())
            })
            .await?;

        let entry = file.into_remote_entry();
        verify_md5(&entry, expected_md5)?;
        Ok(entry)
    }

    /// Resumable create/update for a streaming (`> RESUMABLE_THRESHOLD`, or
    /// caller-requested) body (DESIGN s5.4). Opens a session, pushes
    /// 256-KiB-multiple chunks accumulated from the source stream, and
    /// md5-verifies the completed entry (SPEC s8). A 4xx mid-stream discards
    /// the session and restarts from byte 0 (DESIGN s5.4), bounded so a
    /// persistently-rejecting upload still surfaces an error.
    async fn resumable_upload_stream(
        &self,
        kind: ResumableKind,
        mime: &str,
        len: u64,
        mut stream: Box<dyn futures::Stream<Item = anyhow::Result<Bytes>> + Send + Unpin>,
    ) -> anyhow::Result<RemoteEntry> {
        use futures::StreamExt;

        // We must buffer to honour the 256-KiB-multiple non-final rule AND to
        // recompute md5 + be able to restart from 0 on a session 4xx. The
        // executor's pipeline already bounds the source file's footprint; here
        // we accumulate the whole content once (the stream is single-shot, so
        // a restart cannot re-read it). For the M4 acceptance path (CLI sync,
        // e2e) and the contract's few-hundred-KiB bodies this is bounded; the
        // huge-file pipeline path supplies `UploadBody::Bytes` below the
        // resumable threshold or a re-readable source in M5.
        let mut content = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            content.extend_from_slice(&chunk);
        }
        if content.len() as u64 != len {
            anyhow::bail!(
                "drive: stream length mismatch: declared {len}, got {}",
                content.len()
            );
        }
        let content = Bytes::from(content);
        self.resumable_upload_bytes(kind, mime, content).await
    }

    /// Resumable upload of a fully-buffered body (the restart-capable path).
    async fn resumable_upload_bytes(
        &self,
        kind: ResumableKind,
        mime: &str,
        content: Bytes,
    ) -> anyhow::Result<RemoteEntry> {
        let expected_md5 = md5_of(&content);
        let size = content.len() as u64;
        let chunk = resumable::CHUNK_BYTES;

        // DESIGN s5.4: a 4xx mid-resumable means discard the session and
        // restart from 0 (NEVER resume the old URL). Bound the restart count
        // so a persistently-4xx upload surfaces an error instead of looping.
        let mut restart_attempts = 0u32;
        loop {
            let token = self.bearer().await?;
            let session =
                resumable::open_session(&self.http, &token, clone_kind(&kind), mime, size).await?;

            let mut offset: u64 = 0;
            let mut session_died = false;
            let mut completed: Option<RemoteEntry> = None;
            while offset < size {
                let end = (offset + chunk).min(size);
                let piece = content.slice(offset as usize..end as usize);
                let token = self.bearer().await?;
                let progress =
                    resumable::push_chunk(self.http_stream(), &token, &session, offset, piece)
                        .await?;
                match progress {
                    ResumeProgress::InProgress { received } => {
                        offset = received;
                    }
                    ResumeProgress::Completed(entry) => {
                        completed = Some(entry);
                        break;
                    }
                    ResumeProgress::SessionInvalid => {
                        session_died = true;
                        break;
                    }
                }
            }
            // The zero-length case: open + a single final empty chunk.
            if size == 0 && completed.is_none() && !session_died {
                let token = self.bearer().await?;
                match resumable::push_chunk(self.http_stream(), &token, &session, 0, Bytes::new())
                    .await?
                {
                    ResumeProgress::Completed(entry) => completed = Some(entry),
                    ResumeProgress::SessionInvalid => session_died = true,
                    ResumeProgress::InProgress { .. } => session_died = true,
                }
            }

            if let Some(entry) = completed {
                verify_md5(&entry, expected_md5)?;
                return Ok(entry);
            }
            if session_died {
                restart_attempts += 1;
                if restart_attempts > retry::MAX_RETRIES {
                    return Err(anyhow::Error::new(DriveError::ResumableSessionInvalid));
                }
                warn!(
                    target: TARGET,
                    restart_attempts,
                    "resumable session invalidated mid-upload; restarting from byte 0"
                );
                continue;
            }
            // Loop exited without completing or dying (defensive): retry.
            restart_attempts += 1;
            if restart_attempts > retry::MAX_RETRIES {
                return Err(anyhow::Error::new(DriveError::ResumableSessionInvalid));
            }
        }
    }
}

#[async_trait]
impl RemoteStore for GoogleDriveStore {
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry> {
        // Search by name under the parent (non-trashed folders only). SPEC s3:
        // prefer the Driven-marker folder, else the oldest non-trashed match,
        // else create.
        let q = format!(
            "'{}' in parents and name = '{}' and mimeType = '{}' and trashed = false",
            escape_drive_query(parent_id),
            escape_drive_query(name),
            FOLDER_MIME
        );
        let mut matches = self.list_query(&q).await?;

        if let Some(marked) = matches
            .iter()
            .find(|e| e.app_properties.contains_key(FOLDER_MARKER_KEY))
        {
            return Ok(marked.clone());
        }
        if matches.len() > 1 {
            warn!(
                target: TARGET,
                parent_id = %parent_id,
                name = %name,
                duplicates = matches.len(),
                "ensure_folder found multiple non-marker matches; picking oldest"
            );
        }
        if !matches.is_empty() {
            // Oldest by modified_time (Drive does not return createdTime in
            // our projection; modifiedTime is the closest stable ordering for
            // a freshly-created, never-touched folder).
            matches.sort_by_key(|e| e.modified_time);
            return Ok(matches.remove(0));
        }
        self.create_folder(parent_id, name).await
    }

    async fn list_folder(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>> {
        let q = format!(
            "'{}' in parents and trashed = false",
            escape_drive_query(folder_id)
        );
        self.list_query(&q).await
    }

    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        match body {
            UploadBody::Bytes(content) => {
                if content.len() as u64 >= RESUMABLE_THRESHOLD {
                    self.resumable_upload_bytes(
                        ResumableKind::Create {
                            parent_id: parent_id.to_string(),
                            name: name.to_string(),
                            app_properties,
                        },
                        mime,
                        content,
                    )
                    .await
                } else {
                    self.multipart_upload(
                        None,
                        Some(parent_id),
                        Some(name),
                        mime,
                        content,
                        &app_properties,
                    )
                    .await
                }
            }
            UploadBody::Stream { len, stream } => {
                self.resumable_upload_stream(
                    ResumableKind::Create {
                        parent_id: parent_id.to_string(),
                        name: name.to_string(),
                        app_properties,
                    },
                    mime,
                    len,
                    stream,
                )
                .await
            }
        }
    }

    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        // Drive PATCH /upload (multipart) merges appProperties: keys present in
        // the patch overwrite, keys absent are preserved (the merge the
        // contract scenario asserts). The resumable update endpoint does NOT
        // accept appProperties in the session metadata, so on the resumable
        // path we upload the content first, then apply the patch via a
        // follow-up metadata PATCH (which is also a merge). We carry the
        // existing mime from metadata so the update preserves the file's type.
        let mime = self.metadata(file_id).await?.mime_type;
        match body {
            UploadBody::Bytes(content) => {
                if content.len() as u64 >= RESUMABLE_THRESHOLD {
                    let entry = self
                        .resumable_upload_bytes(
                            ResumableKind::Update {
                                file_id: file_id.to_string(),
                            },
                            &mime,
                            content,
                        )
                        .await?;
                    self.apply_props_patch(entry, file_id, &app_properties_patch)
                        .await
                } else {
                    self.multipart_upload(
                        Some(file_id),
                        None,
                        None,
                        &mime,
                        content,
                        &app_properties_patch,
                    )
                    .await
                }
            }
            UploadBody::Stream { len, stream } => {
                let entry = self
                    .resumable_upload_stream(
                        ResumableKind::Update {
                            file_id: file_id.to_string(),
                        },
                        &mime,
                        len,
                        stream,
                    )
                    .await?;
                self.apply_props_patch(entry, file_id, &app_properties_patch)
                    .await
            }
        }
    }

    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        let token = self.bearer().await?;
        resumable::open_session(&self.http, &token, kind, mime, size).await
    }

    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        // SPEC s3 / DESIGN s5.4: non-final chunks MUST be a multiple of
        // 256 KiB. Enforce at the trait layer so the contract's
        // `scenario_resumable_non_multiple_rejected` returns SessionInvalid
        // exactly as the fake does (matching the wire-level 400 Drive returns).
        let is_final = offset + chunk.len() as u64 == session.size;
        if !is_final && (chunk.len() as u64) % resumable::CHUNK_MULTIPLE != 0 {
            return Ok(ResumeProgress::SessionInvalid);
        }
        let token = self.bearer().await?;
        resumable::push_chunk(self.http_stream(), &token, session, offset, chunk).await
    }

    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        let body = json_body(&serde_json::json!({ "trashed": true }))?;
        let result: anyhow::Result<DriveFile> = self
            .send_json(|token| {
                self.http
                    .patch(format!("{DRIVE_API_BASE}/files/{file_id}"))
                    .query(&[("fields", "id,trashed")])
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                // 404 -> already gone, treated as success (SPEC s3 `trash`).
                if is_not_found(&e) {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        let file: DriveFile = self
            .send_json(|token| {
                self.http
                    .get(format!("{DRIVE_API_BASE}/files/{file_id}"))
                    .query(&[("fields", pagination::FILE_FIELDS)])
                    .bearer_auth(token)
            })
            .await?;
        Ok(file.into_remote_entry())
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        // Stream straight off the wire (no buffering). We do NOT wrap this in
        // `with_retry` because a partially-consumed stream cannot be replayed;
        // the executor's restore sink re-requests on a mid-stream failure.
        let token = self.bearer().await?;
        let resp = self
            .http_stream()
            .get(format!("{DRIVE_API_BASE}/files/{file_id}"))
            .query(&[("alt", "media")])
            .bearer_auth(token)
            .send()
            .await
            .map_err(DriveError::from_transport)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let retry_after = parse_retry_after(&resp);
            let bytes = resp.bytes().await.map_err(DriveError::from_transport)?;
            return Err(anyhow::Error::new(DriveError::from_response(
                status,
                &bytes,
                retry_after,
            )));
        }
        // Map the byte stream into an AsyncRead the trait expects. We
        // hand-roll the adapter (no `tokio-util` dep) so the response body
        // streams straight off the wire into the restore sink. `bytes_stream()`
        // is not guaranteed `Unpin`, so box-pin it (a `Pin<Box<..>>` is `Unpin`).
        let stream: BoxByteStream = Box::pin(resp.bytes_stream());
        let reader = StreamingDownloadReader::new(stream);
        Ok(DownloadStream(Box::new(reader)))
    }

    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        // DESIGN s5.6 reconciliation: the orphan we adopt is a LIVE
        // (non-trashed) child of the parent whose appProperties carry the
        // op uuid. Drive supports `appProperties has { key='..' and value='..' }`.
        let q = format!(
            "'{}' in parents and trashed = false and appProperties has {{ key='{}' and value='{}' }}",
            escape_drive_query(parent_id),
            escape_drive_query(CLIENT_OP_UUID_KEY),
            escape_drive_query(op_uuid),
        );
        let mut matches = self.list_query(&q).await?;
        if matches.is_empty() {
            return Ok(None);
        }
        if matches.len() > 1 {
            warn!(
                target: TARGET,
                parent_id = %parent_id,
                op_uuid = %op_uuid,
                duplicates = matches.len(),
                "find_by_op_uuid found multiple matches; returning most-recent by modifiedTime"
            );
        }
        // Most-recent by modifiedTime.
        matches.sort_by_key(|e| std::cmp::Reverse(e.modified_time));
        Ok(Some(matches.remove(0)))
    }

    async fn about(&self) -> anyhow::Result<AboutInfo> {
        let resp: AboutResponse = self
            .send_json(|token| {
                self.http
                    .get(format!("{DRIVE_API_BASE}/about"))
                    .query(&[("fields", "storageQuota")])
                    .bearer_auth(token)
            })
            .await?;
        let q = resp.storage_quota;
        Ok(AboutInfo {
            limit: q.limit.as_deref().and_then(|s| s.parse::<u64>().ok()),
            usage: q
                .usage
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
            usage_in_drive: q
                .usage_in_drive
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
            usage_in_drive_trash: q
                .usage_in_drive_trash
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
        })
    }
}

impl GoogleDriveStore {
    /// After a resumable update, applies the appProperties patch via a
    /// metadata `PATCH /files/{id}` (the resumable update endpoint does not
    /// accept appProperties at session-open time). Drive merges the patch into
    /// the existing appProperties, preserving the keys not in the patch - the
    /// merge-semantics the contract requires. A no-op (returns the entry
    /// unchanged) when the patch is empty, saving a round-trip.
    async fn apply_props_patch(
        &self,
        entry: RemoteEntry,
        file_id: &str,
        patch: &HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        if patch.is_empty() {
            return Ok(entry);
        }
        let body = json_body(&serde_json::json!({ "appProperties": patch }))?;
        let file: DriveFile = self
            .send_json(|token| {
                self.http
                    .patch(format!("{DRIVE_API_BASE}/files/{file_id}"))
                    .query(&[("fields", pagination::FILE_FIELDS)])
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;
        Ok(file.into_remote_entry())
    }
}

// ---------------------------------------------------------------------------
// Free helpers.
// ---------------------------------------------------------------------------

/// Builds the metadata Drive client with the DESIGN s5.8.4 timeouts.
pub(crate) fn build_meta_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(META_TOTAL_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| anyhow::anyhow!("drive: failed to build metadata client: {e}"))
}

/// Builds the streaming Drive client (resumable chunk PUT + download): no
/// overall request cap, only the per-byte idle timeout (DESIGN s5.8.4 `*`).
pub(crate) fn build_stream_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| anyhow::anyhow!("drive: failed to build streaming client: {e}"))
}

/// The `Retry-After` header (in ms) off a response, if present and parseable
/// (Drive sends seconds; we convert).
pub(crate) fn parse_retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
}

/// Whether an error is a Drive 404 (used to make `trash` idempotent).
fn is_not_found(err: &anyhow::Error) -> bool {
    // `DriveError::from_response` embeds `drive HTTP 404` in the source chain.
    err.chain()
        .any(|c| c.to_string().contains("drive HTTP 404"))
}

/// Serializes a JSON value to a [`Bytes`] body. Used instead of reqwest's
/// `.json()` because driven-drive does NOT enable reqwest's `json` feature
/// (workspace reqwest is `rustls-tls,http2,stream` only); the caller sets the
/// `Content-Type: application/json` header explicitly.
pub(crate) fn json_body(value: &serde_json::Value) -> anyhow::Result<Bytes> {
    let v = serde_json::to_vec(value)
        .map_err(|e| anyhow::anyhow!("drive: failed to serialize request JSON: {e}"))?;
    Ok(Bytes::from(v))
}

/// md5 of a byte slice (Drive's `md5Checksum` is over the exact bytes sent;
/// DESIGN s5.4 / s7.1 - ciphertext when encrypted, plaintext otherwise).
pub(crate) fn md5_of(bytes: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(bytes);
    let out = h.finalize();
    let mut md5 = [0u8; 16];
    md5.copy_from_slice(&out);
    md5
}

/// Verifies the entry's md5 against the bytes Driven uploaded (SPEC s8). A
/// folder / a missing md5 on a non-empty body is treated as a mismatch; a
/// match returns `Ok`.
pub(crate) fn verify_md5(entry: &RemoteEntry, expected: [u8; 16]) -> anyhow::Result<()> {
    match entry.md5 {
        Some(actual) if actual == expected => Ok(()),
        Some(_) => Err(anyhow::Error::new(DriveError::ChecksumMismatch)),
        None => {
            // Drive returns no md5 for a 0-byte file; the empty-content md5 is
            // d41d8cd98f00b204e9800998ecf8427e. Accept only that case.
            if expected == md5_of(&[]) {
                Ok(())
            } else {
                Err(anyhow::Error::new(DriveError::ChecksumMismatch))
            }
        }
    }
}

/// Escapes a value for embedding inside a single-quoted Drive query literal
/// (`q=` parameter). Drive query strings escape `\` and `'` with a backslash.
pub(crate) fn escape_drive_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Assembles a `multipart/related` body: JSON metadata part + raw content
/// part, each delimited by `--<boundary>`.
fn build_multipart_related(
    boundary: &str,
    metadata_json: &[u8],
    content_mime: &str,
    content: &[u8],
) -> Bytes {
    let mut body = Vec::with_capacity(metadata_json.len() + content.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata_json);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Type: {content_mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Bytes::from(body)
}

/// A v4 UUID string (boundary token / op-uuid plumbing).
fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Streaming download adapter.
// ---------------------------------------------------------------------------

/// A boxed, pinned `reqwest` byte stream. `reqwest::Response::bytes_stream()`
/// is not guaranteed `Unpin`, so we box-pin it (a `Pin<Box<..>>` is `Unpin`).
type BoxByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// An [`AsyncRead`] over a `reqwest` byte stream (the [`download`] body),
/// hand-rolled so driven-drive needs no `tokio-util` dependency. Holds the
/// current partially-consumed chunk and polls the underlying stream for the
/// next when it drains. A stream error surfaces as an `io::Error` so the
/// restore sink's `tokio::io::copy` sees it.
struct StreamingDownloadReader {
    stream: BoxByteStream,
    current: Bytes,
    done: bool,
}

impl StreamingDownloadReader {
    fn new(stream: BoxByteStream) -> Self {
        Self {
            stream,
            current: Bytes::new(),
            done: false,
        }
    }
}

impl AsyncRead for StreamingDownloadReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.current.is_empty() {
                let n = self.current.len().min(buf.remaining());
                let chunk = self.current.split_to(n);
                buf.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            if self.done {
                return Poll::Ready(Ok(()));
            }
            match self.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.current = bytes;
                    // Loop to copy from the freshly-fetched chunk.
                }
                Poll::Ready(Some(Err(e))) => {
                    self.done = true;
                    return Poll::Ready(Err(std::io::Error::other(e)));
                }
                Poll::Ready(None) => {
                    self.done = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// `ResumableKind` is not `Clone` (it carries owned strings); the resumable
/// restart loop needs a copy per attempt.
pub(crate) fn clone_kind(kind: &ResumableKind) -> ResumableKind {
    match kind {
        ResumableKind::Create {
            parent_id,
            name,
            app_properties,
        } => ResumableKind::Create {
            parent_id: parent_id.clone(),
            name: name.clone(),
            app_properties: app_properties.clone(),
        },
        ResumableKind::Update { file_id } => ResumableKind::Update {
            file_id: file_id.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses_to_unix_ms() {
        // 2024-01-01T00:00:00Z == 1704067200000 ms.
        assert_eq!(
            parse_rfc3339_to_unix_ms("2024-01-01T00:00:00Z"),
            Some(1_704_067_200_000)
        );
        // With millis.
        assert_eq!(
            parse_rfc3339_to_unix_ms("2024-01-01T00:00:00.500Z"),
            Some(1_704_067_200_500)
        );
        // The Unix epoch itself.
        assert_eq!(parse_rfc3339_to_unix_ms("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn md5_hex_round_trips() {
        let md5 = md5_of(b"hi");
        let hex = hex::encode(md5);
        assert_eq!(parse_md5_hex(&hex), Some(md5));
        assert_eq!(parse_md5_hex("not-hex"), None);
        assert_eq!(parse_md5_hex("ab"), None);
    }

    #[test]
    fn classified_display_carries_spec_codes() {
        // The M3 executor's string matcher relies on these substrings.
        let rl = DriveError::Classified {
            kind: DriveErrorClassification::RateLimited { retry_after_ms: 5 },
            source: anyhow::anyhow!("x"),
        };
        assert!(rl.to_string().contains("rate_limited"));

        let daily = DriveError::Classified {
            kind: DriveErrorClassification::DailyQuota,
            source: anyhow::anyhow!("x"),
        };
        assert!(daily.to_string().contains("daily"));

        let storage = DriveError::Classified {
            kind: DriveErrorClassification::StorageQuota,
            source: anyhow::anyhow!("x"),
        };
        assert!(storage.to_string().contains("quota_exhausted"));

        let auth = DriveError::Classified {
            kind: DriveErrorClassification::AuthInvalidGrant,
            source: anyhow::anyhow!("x"),
        };
        assert!(auth.to_string().contains("invalid_grant"));

        let net = DriveError::Classified {
            kind: DriveErrorClassification::Network,
            source: anyhow::anyhow!("x"),
        };
        assert!(net.to_string().contains("intermittent"));

        let t5 = DriveError::Classified {
            kind: DriveErrorClassification::Transient5xx,
            source: anyhow::anyhow!("x"),
        };
        assert!(t5.to_string().contains("unreachable"));

        assert!(DriveError::DestFolderMissing
            .to_string()
            .contains("dest_folder_missing"));
        assert!(DriveError::DestFolderPermissionDenied
            .to_string()
            .contains("dest_folder_permission_denied"));
    }

    #[test]
    fn classification_of_round_trips() {
        let e = anyhow::Error::new(DriveError::Classified {
            kind: DriveErrorClassification::DailyQuota,
            source: anyhow::anyhow!("x"),
        });
        assert_eq!(
            classification_of(&e),
            Some(DriveErrorClassification::DailyQuota)
        );
        assert_eq!(classification_of(&anyhow::anyhow!("plain")), None);
    }

    #[test]
    fn escape_drive_query_escapes_quotes() {
        assert_eq!(escape_drive_query("it's"), "it\\'s");
        assert_eq!(escape_drive_query(r"a\b"), r"a\\b");
    }
}
