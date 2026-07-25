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
use crate::remote_store::{DriveContext, RemoteEntry};

/// The `fields=` projection Driven requests for every `files.list` /
/// `files.get` call (ROADMAP M4 "field selection so we don't pull more than
/// we need"). Covers exactly the [`RemoteEntry`] shape: id, name, parents,
/// size, md5Checksum, mimeType, modifiedTime, trashed, appProperties.
pub const FILE_FIELDS: &str =
    "id,name,parents,size,md5Checksum,mimeType,modifiedTime,trashed,appProperties";

/// The MINIMAL `fields=` projection: just the object id.
///
/// [`list_ids_all`] uses this for the remote-existence audit, which enumerates
/// every object one source owns purely to learn WHICH ids are still alive. The
/// full [`FILE_FIELDS`] projection would pull nine fields per row and throw
/// eight of them away - on a source with hundreds of thousands of objects that
/// is megabytes of wire and JSON parsing for data nothing reads.
pub const ID_ONLY_FIELDS: &str = "id";

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
    drive_context: &DriveContext,
) -> anyhow::Result<Vec<RemoteEntry>> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut page_token: Option<String> = None;
    loop {
        let (entries, next) =
            list_page(http, access_token, q, page_token.as_deref(), drive_context).await?;
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

/// Runs the full `files.list` pagination loop for `q`, collecting ONLY the
/// object ids into a set (the id-only counterpart of [`list_all`]).
///
/// Backs [`crate::remote_store::RemoteStore::list_source_object_ids`], the
/// remote-existence audit's enumeration primitive. Requests
/// [`ID_ONLY_FIELDS`] instead of the full [`FILE_FIELDS`] projection, because
/// the audit only needs to know which ids are still alive.
///
/// COMPLETENESS IS THE CONTRACT. The audit treats "recorded id absent from
/// this set" as proof the object is gone and heals the row, so a listing that
/// stopped early would read as a mass deletion and re-upload the whole source.
/// Any page error therefore propagates as `Err` and the caller MUST abort
/// without writing anything - a returned set is always the COMPLETE live-id
/// set, never a partial one.
pub async fn list_ids_all(
    http: &reqwest::Client,
    access_token: &str,
    q: &str,
    drive_context: &DriveContext,
) -> anyhow::Result<HashSet<String>> {
    let mut out: HashSet<String> = HashSet::new();
    let mut page_token: Option<String> = None;
    loop {
        let (ids, next) =
            list_ids_page(http, access_token, q, page_token.as_deref(), drive_context).await?;
        out.extend(ids);
        match next {
            Some(tok) if !tok.is_empty() => page_token = Some(tok),
            _ => break,
        }
    }
    Ok(out)
}

/// One page of the [`list_ids_all`] loop: the ids on this page plus the
/// `nextPageToken` (or `None` when this was the last page).
async fn list_ids_page(
    http: &reqwest::Client,
    access_token: &str,
    q: &str,
    page_token: Option<&str>,
    drive_context: &DriveContext,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
    let query = list_query_params_with_fields(q, page_token, drive_context, ID_ONLY_FIELDS);

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

    let parsed: IdListResponse = serde_json::from_slice(&body)
        .map_err(|e| anyhow::anyhow!("drive: failed to parse files.list id page: {e}"))?;
    Ok((
        parsed.files.into_iter().map(|f| f.id).collect(),
        parsed.next_page_token,
    ))
}

/// The id-only `files.list` response page. A dedicated shape rather than
/// [`ListResponse`]: under [`ID_ONLY_FIELDS`] every other [`DriveFile`] field
/// is absent from the wire, so decoding through the full type would depend on
/// each of its fields staying `#[serde(default)]` forever.
#[derive(Debug, Deserialize)]
struct IdListResponse {
    #[serde(default)]
    files: Vec<IdOnlyFile>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

/// One row of an [`ID_ONLY_FIELDS`] listing.
#[derive(Debug, Deserialize)]
struct IdOnlyFile {
    id: String,
}

/// Builds the `files.list` query parameters for one page (issue #7 Shared
/// Drives). Split out as a PURE function so the exact wire params - and how
/// they differ between My Drive and a Shared Drive - are unit-testable without
/// standing up an HTTP server.
///
/// Common to both contexts: the `files(..)` field projection, `pageSize`,
/// `spaces=drive` (the Drive space, orthogonal to My-Drive-vs-Shared-Drive),
/// the query `q`, an optional `pageToken`, and `supportsAllDrives=true` (the
/// caller supports both My Drives and Shared Drives; harmless for My Drive).
///
/// - [`DriveContext::MyDrive`]: adds `corpora=user` (the My-Drive default).
/// - [`DriveContext::SharedDrive`]: adds `corpora=drive` + `driveId=<id>` +
///   `includeItemsFromAllDrives=true`, which is the ONLY combination that
///   returns objects living inside a Shared Drive (`corpora=user` hides them).
pub fn list_query_params(
    q: &str,
    page_token: Option<&str>,
    drive_context: &DriveContext,
) -> Vec<(&'static str, String)> {
    list_query_params_with_fields(q, page_token, drive_context, FILE_FIELDS)
}

/// [`list_query_params`] with an explicit per-file field projection, so a
/// caller that needs only some of a file's fields does not pay for the full
/// [`FILE_FIELDS`] shape ([`list_ids_all`] passes [`ID_ONLY_FIELDS`]).
/// Everything else about the request - corpus scoping, `pageSize`, `spaces`,
/// `supportsAllDrives`, the page token - is identical.
pub fn list_query_params_with_fields(
    q: &str,
    page_token: Option<&str>,
    drive_context: &DriveContext,
    file_fields: &str,
) -> Vec<(&'static str, String)> {
    // Field selection for a LIST nests the file projection under `files(..)`
    // and adds the page-token field.
    let fields = format!("nextPageToken,files({file_fields})");
    let mut query: Vec<(&'static str, String)> = vec![
        ("q", q.to_string()),
        ("fields", fields),
        ("pageSize", "1000".to_string()),
        // The Drive `spaces` (not Photos / AppData) - orthogonal to whether the
        // corpus is My Drive or a Shared Drive.
        ("spaces", "drive".to_string()),
        // Sent unconditionally: this client supports both My Drives and Shared
        // Drives. Harmless for a My-Drive-only listing.
        ("supportsAllDrives", "true".to_string()),
    ];
    match drive_context {
        DriveContext::MyDrive => {
            // The My-Drive corpus (the V1 default).
            query.push(("corpora", "user".to_string()));
        }
        DriveContext::SharedDrive { drive_id } => {
            // Confine the search to the one Shared Drive. `corpora=drive` +
            // `driveId` + `includeItemsFromAllDrives=true` is the required
            // combination for a Shared Drive listing (Drive API guide
            // "Search for content on a shared drive").
            query.push(("corpora", "drive".to_string()));
            query.push(("driveId", drive_id.clone()));
            query.push(("includeItemsFromAllDrives", "true".to_string()));
        }
    }
    if let Some(tok) = page_token {
        query.push(("pageToken", tok.to_string()));
    }
    query
}

/// Fetches a single `files.list` page, returning the entries and the
/// `nextPageToken` (or `None` when this is the last page). The building block
/// [`list_all`] loops over.
pub async fn list_page(
    http: &reqwest::Client,
    access_token: &str,
    q: &str,
    page_token: Option<&str>,
    drive_context: &DriveContext,
) -> anyhow::Result<(Vec<RemoteEntry>, Option<String>)> {
    let query = list_query_params(q, page_token, drive_context);

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

    /// Look up the single value for `key` in a built query param list.
    fn param<'a>(query: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
        query
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn list_params_my_drive_uses_corpora_user_and_supports_all_drives() {
        // Issue #7: My Drive keeps `corpora=user`, gains `supportsAllDrives=true`
        // unconditionally, and NEVER carries a driveId / includeItemsFromAllDrives.
        let q = "'root' in parents and trashed = false";
        let query = list_query_params(q, None, &DriveContext::MyDrive);
        assert_eq!(param(&query, "q"), Some(q));
        assert_eq!(param(&query, "corpora"), Some("user"));
        assert_eq!(param(&query, "supportsAllDrives"), Some("true"));
        assert_eq!(param(&query, "spaces"), Some("drive"));
        assert_eq!(param(&query, "driveId"), None);
        assert_eq!(param(&query, "includeItemsFromAllDrives"), None);
        assert_eq!(param(&query, "pageToken"), None);
    }

    #[test]
    fn list_params_shared_drive_scopes_to_drive_id() {
        // Issue #7: a Shared Drive listing switches to `corpora=drive` and adds
        // `driveId` + `includeItemsFromAllDrives=true` (the only combination
        // that returns objects inside a Shared Drive), keeping supportsAllDrives.
        let q = "'FOLDER' in parents and trashed = false";
        let ctx = DriveContext::SharedDrive {
            drive_id: "0ADriveIdXYZ".to_string(),
        };
        let query = list_query_params(q, Some("tok42"), &ctx);
        assert_eq!(param(&query, "corpora"), Some("drive"));
        assert_eq!(param(&query, "driveId"), Some("0ADriveIdXYZ"));
        assert_eq!(param(&query, "includeItemsFromAllDrives"), Some("true"));
        assert_eq!(param(&query, "supportsAllDrives"), Some("true"));
        assert_eq!(param(&query, "spaces"), Some("drive"));
        // corpora=user must NOT be present alongside corpora=drive.
        assert_eq!(param(&query, "corpora"), Some("drive"));
        assert_eq!(param(&query, "pageToken"), Some("tok42"));
    }

    /// The audit's enumeration asks for ONLY the id. The projection must be
    /// exactly `nextPageToken,files(id)` - not the nine-field `FILE_FIELDS`
    /// shape - while every other request parameter stays identical to a normal
    /// listing (the audit is a plain `files.list`, just a leaner one).
    #[test]
    fn id_only_params_request_just_the_id_and_keep_everything_else() {
        let q = "appProperties has { key='driven.source_id' and value='s1' } and trashed = false";
        let full = list_query_params(q, None, &DriveContext::MyDrive);
        let ids = list_query_params_with_fields(q, None, &DriveContext::MyDrive, ID_ONLY_FIELDS);

        assert_eq!(param(&ids, "fields"), Some("nextPageToken,files(id)"));
        assert_ne!(
            param(&ids, "fields"),
            param(&full, "fields"),
            "the id-only projection must not silently inherit FILE_FIELDS"
        );
        // Everything that is NOT the projection is unchanged.
        for key in ["q", "pageSize", "spaces", "supportsAllDrives", "corpora"] {
            assert_eq!(
                param(&ids, key),
                param(&full, key),
                "{key} must match the full-projection listing"
            );
        }
    }

    /// A Shared Drive audit listing keeps the `corpora=drive` + `driveId` +
    /// `includeItemsFromAllDrives` combination - the only one that returns
    /// objects living inside a Shared Drive. Without it the audit would see an
    /// EMPTY live set for every Shared-Drive source and declare every recorded
    /// id dead.
    #[test]
    fn id_only_params_still_scope_to_a_shared_drive() {
        let ctx = DriveContext::SharedDrive {
            drive_id: "0ADriveIdXYZ".to_string(),
        };
        let query = list_query_params_with_fields("q", Some("tok9"), &ctx, ID_ONLY_FIELDS);
        assert_eq!(param(&query, "corpora"), Some("drive"));
        assert_eq!(param(&query, "driveId"), Some("0ADriveIdXYZ"));
        assert_eq!(param(&query, "includeItemsFromAllDrives"), Some("true"));
        assert_eq!(param(&query, "pageToken"), Some("tok9"));
        assert_eq!(param(&query, "fields"), Some("nextPageToken,files(id)"));
    }

    /// An id-only page decodes from a body carrying ONLY `id` per file - the
    /// shape `fields=nextPageToken,files(id)` actually returns. Decoding it
    /// through the full `DriveFile` type would couple the audit to every one
    /// of that struct's fields staying optional.
    #[test]
    fn id_list_response_parses_id_only_rows() {
        let page = br#"{"nextPageToken":"tok7","files":[{"id":"a"},{"id":"b"}]}"#;
        let r: IdListResponse = serde_json::from_slice(page).unwrap();
        assert_eq!(r.next_page_token.as_deref(), Some("tok7"));
        assert_eq!(
            r.files.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        // Last page: no token, and an empty `files` array is legal.
        let last = br#"{"files":[]}"#;
        let r: IdListResponse = serde_json::from_slice(last).unwrap();
        assert!(r.next_page_token.is_none());
        assert!(r.files.is_empty());
    }

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
