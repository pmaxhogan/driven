//! The `files.list` `pageToken` pagination loop (SPEC s3, ROADMAP M4).
//!
//! Drive's `files.list` returns at most one page plus a `nextPageToken`;
//! [`list_all`] drives the loop until the token is exhausted, applying field
//! selection (`fields=`) so we never pull more than we need (ROADMAP M4).
//!
//! Drive `files.list` pagination is by `pageToken` (the `nextPageToken` of the
//! previous page), NOT a numeric page or a `skip` offset. We loop until
//! `nextPageToken` is absent, deduping defensively by id in case Drive repeats
//! a row across a page boundary.

use std::collections::HashSet;

use serde::Deserialize;

use super::{DriveError, DriveFile};
use crate::remote_store::RemoteEntry;

/// The `fields=` projection Driven requests for every `files.list` /
/// `files.get` call (ROADMAP M4 "field selection so we don't pull more than
/// we need"). Covers exactly the [`RemoteEntry`] shape: id, name, parents,
/// size, md5Checksum, mimeType, modifiedTime, trashed, appProperties.
pub const FILE_FIELDS: &str =
    "id,name,parents,size,md5Checksum,mimeType,modifiedTime,trashed,appProperties";

/// The `files.list` response page shape.
#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

/// Runs the full `files.list` pagination loop for a Drive query `q`,
/// collecting every page into one `Vec<RemoteEntry>` (SPEC s3, ROADMAP M4).
///
/// Follows `nextPageToken` until it is absent, requesting [`FILE_FIELDS`] for
/// each page. `q` is the Drive query string (e.g. `'<parent>' in parents and
/// trashed = false`). Dedupes by id across pages (Drive can rarely repeat a
/// row on a page boundary).
pub async fn list_all(
    http: &reqwest::Client,
    access_token: &str,
    q: &str,
) -> anyhow::Result<Vec<RemoteEntry>> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut page_token: Option<String> = None;
    loop {
        let (entries, next) = list_page(http, access_token, q, page_token.as_deref()).await?;
        for e in entries {
            if seen.insert(e.id.clone()) {
                out.push(e);
            }
        }
        match next {
            Some(tok) if !tok.is_empty() => page_token = Some(tok),
            _ => break,
        }
    }
    Ok(out)
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
    // Field selection for a LIST nests the file projection under `files(..)`
    // and adds the page-token field.
    let fields = format!("nextPageToken,files({FILE_FIELDS})");
    let mut query: Vec<(&str, String)> = vec![
        ("q", q.to_string()),
        ("fields", fields),
        ("pageSize", "1000".to_string()),
        // Confine to My Drive (V1 scope; no Shared Drives, per fake docs).
        ("spaces", "drive".to_string()),
        ("corpora", "user".to_string()),
    ];
    if let Some(tok) = page_token {
        query.push(("pageToken", tok.to_string()));
    }

    let resp = http
        .get(format!("{}/files", super::DRIVE_API_BASE))
        .query(&query)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(DriveError::from_transport)?;

    let status = resp.status().as_u16();
    let retry_after = super::parse_retry_after(&resp);
    let body = resp.bytes().await.map_err(DriveError::from_transport)?;
    if !(200..300).contains(&status) {
        return Err(anyhow::Error::new(DriveError::from_response(
            status,
            &body,
            retry_after,
        )));
    }

    let parsed: ListResponse = serde_json::from_slice(&body)
        .map_err(|e| anyhow::anyhow!("drive: failed to parse files.list response: {e}"))?;
    let entries = parsed
        .files
        .into_iter()
        .map(DriveFile::into_remote_entry)
        .collect();
    Ok((entries, parsed.next_page_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_response_parses_with_and_without_token() {
        let with_token = br#"{
            "nextPageToken": "tok123",
            "files": [
                {"id":"a","name":"x.txt","mimeType":"text/plain","size":"2","trashed":false}
            ]
        }"#;
        let r: ListResponse = serde_json::from_slice(with_token).unwrap();
        assert_eq!(r.next_page_token.as_deref(), Some("tok123"));
        assert_eq!(r.files.len(), 1);

        let last = br#"{"files":[]}"#;
        let r: ListResponse = serde_json::from_slice(last).unwrap();
        assert!(r.next_page_token.is_none());
        assert!(r.files.is_empty());
    }
}
