//! The `files.list` `pageToken` pagination loop (SPEC s3, ROADMAP M4).
//!
//! Drive's `files.list` returns at most one page plus a `nextPageToken`;
//! [`list_all`] drives the loop until the token is exhausted, applying field
//! selection (`fields=`) so we never pull more than we need (ROADMAP M4).
//!
//! M4 scaffold: signatures only; bodies are `todo!()`.

use crate::remote_store::RemoteEntry;

/// The `fields=` projection Driven requests for every `files.list` /
/// `files.get` call (ROADMAP M4 "field selection so we don't pull more than
/// we need"). Covers exactly the [`RemoteEntry`] shape: id, name, parents,
/// size, md5Checksum, mimeType, modifiedTime, trashed, appProperties.
pub const FILE_FIELDS: &str =
    "id,name,parents,size,md5Checksum,mimeType,modifiedTime,trashed,appProperties";

/// Runs the full `files.list` pagination loop for a Drive query `q`,
/// collecting every page into one `Vec<RemoteEntry>` (SPEC s3, ROADMAP M4).
///
/// Follows `nextPageToken` until it is absent, requesting [`FILE_FIELDS`] for
/// each page. `q` is the Drive query string (e.g. `'<parent>' in parents and
/// trashed = false`).
pub async fn list_all(
    http: &reqwest::Client,
    access_token: &str,
    q: &str,
) -> anyhow::Result<Vec<RemoteEntry>> {
    let _ = (http, access_token, q);
    todo!("M4 implement: files.list pageToken loop with FILE_FIELDS projection")
}

/// Fetches a single `files.list` page, returning the entries and the
/// `nextPageToken` (or `None` when this is the last page). The building block
/// [`list_all`] loops over.
pub async fn list_page(
    http: &reqwest::Client,
    access_token: &str,
    q: &str,
    page_token: Option<&str>,
) -> anyhow::Result<(Vec<RemoteEntry>, Option<String>)> {
    let _ = (http, access_token, q, page_token);
    todo!("M4 implement: one files.list page -> (entries, nextPageToken)")
}
