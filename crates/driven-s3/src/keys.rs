//! The object-key layout, and how Driven's `app_properties` survive a round
//! trip through S3 user metadata.
//!
//! ## Folders
//!
//! S3 has no directories - a key is one opaque string. Driven's [`RemoteStore`]
//! contract is folder-shaped (`ensure_folder` returns an id that later becomes
//! a `parent_id`), so this backend maps a "folder id" onto a **key prefix
//! ending in `/`**:
//!
//! ```text
//! root folder id        = the configured prefix ("" for the bucket root)
//! ensure_folder(p, "x") = "<p>x/"          (no object is created)
//! create(p, "name")     = key "<p>name"
//! ```
//!
//! `ensure_folder` therefore performs NO request: a prefix always "exists" in
//! S3, so there is nothing to create and nothing to race. That also means no
//! zero-byte directory-marker objects are written - they would show up in
//! `list_source_object_ids` as objects Driven owns but has no `file_state` row
//! for, and the remote-existence audit would try to heal them forever.
//!
//! A file's id is its FULL key, which is what `file_state.drive_file_id` stores.
//! Unlike Drive (where the id is stable across renames) an S3 key IS the name,
//! so a rename is a delete + create - exactly what the executor already plans,
//! since it keys `file_state` by relative path.
//!
//! ## `app_properties`
//!
//! S3 user metadata is carried in `x-amz-meta-*` HTTP headers, which constrains
//! keys to HTTP token characters and values to ASCII. Driven's keys contain dots
//! (`driven.source_id`) and the trait permits arbitrary caller keys, so rather
//! than invent a lossy key-mangling scheme, the whole map is serialized to JSON
//! and stored base64 in ONE header, [`PROPS_METADATA_KEY`]. That is lossless for
//! any key/value the trait allows, costs ~1 header, and stays far inside S3's
//! 2 KiB user-metadata budget for the five short `driven.*` keys Driven
//! actually stamps.
//!
//! [`RemoteStore`]: driven_remote::remote_store::RemoteStore

use std::collections::HashMap;

use base64::Engine as _;

/// The `x-amz-meta-` suffix carrying the base64 JSON `app_properties` map.
///
/// Part of the stored format: an object written by one Driven build must be
/// readable by the next, so this string never changes.
pub const PROPS_METADATA_KEY: &str = "driven-props";

/// The `x-amz-meta-` suffix carrying the hex MD5 of the object's stored bytes.
///
/// Written on single-`PutObject` uploads, where it is redundant with the ETag
/// but cheap. NOT written by the multipart path: S3 fixes user metadata at
/// `CreateMultipartUpload` time, before any byte has been hashed. See
/// `store::S3Store::metadata` for what a reader gets instead.
pub const CONTENT_MD5_METADATA_KEY: &str = "driven-md5";

/// Joins a folder-id prefix and a child name into an object key.
///
/// `parent` is a prefix that is either empty (the bucket root) or `/`-
/// terminated; this tolerates a missing trailing slash so a hand-edited config
/// cannot silently produce `foobar` from `foo` + `bar`.
pub fn join_key(parent: &str, name: &str) -> String {
    let name = name.trim_start_matches('/');
    if parent.is_empty() {
        return name.to_string();
    }
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The folder id for `name` under the `parent` folder id: the joined key with a
/// trailing slash.
pub fn folder_id(parent: &str, name: &str) -> String {
    let mut id = join_key(parent, name.trim_end_matches('/'));
    if !id.ends_with('/') {
        id.push('/');
    }
    id
}

/// The display name of an object key or folder prefix - the last non-empty path
/// segment. Returns `""` for the bucket root.
pub fn base_name(key: &str) -> &str {
    key.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// The parent folder id of an object key or folder prefix (`""` at the root).
pub fn parent_of(key: &str) -> String {
    let trimmed = key.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[..=i].to_string(),
        None => String::new(),
    }
}

/// Encode an `app_properties` map for the [`PROPS_METADATA_KEY`] header.
///
/// Empty maps encode to `None` so no header is sent at all - an object with no
/// properties is indistinguishable from one written before this backend existed,
/// which is the correct reading.
pub fn encode_props(props: &HashMap<String, String>) -> anyhow::Result<Option<String>> {
    if props.is_empty() {
        return Ok(None);
    }
    let json = serde_json::to_vec(props)
        .map_err(|e| anyhow::anyhow!("failed to encode app properties: {e}"))?;
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(json)))
}

/// Decode a [`PROPS_METADATA_KEY`] header value.
///
/// A missing header is an empty map. A CORRUPT header is also an empty map, with
/// a warning: the properties are Driven's own identity stamp, and an object
/// whose stamp cannot be read is correctly treated as "not ours" by
/// `find_by_op_uuid` / `list_source_object_ids`. Failing the whole listing
/// instead would let one hand-edited object wedge every audit.
pub fn decode_props(raw: Option<&str>) -> HashMap<String, String> {
    let Some(raw) = raw else {
        return HashMap::new();
    };
    let decoded = match base64::engine::general_purpose::STANDARD.decode(raw) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                target: crate::TARGET,
                %err,
                "ignoring an object whose driven-props metadata is not valid base64"
            );
            return HashMap::new();
        }
    };
    match serde_json::from_slice(&decoded) {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                target: crate::TARGET,
                %err,
                "ignoring an object whose driven-props metadata is not a valid JSON map"
            );
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_join_under_the_root_and_under_a_prefix() {
        assert_eq!(join_key("", "a.txt"), "a.txt");
        assert_eq!(join_key("p/", "a.txt"), "p/a.txt");
        assert_eq!(join_key("p/q/", "a.txt"), "p/q/a.txt");
        // A prefix missing its trailing slash must not glue names together.
        assert_eq!(join_key("p", "a.txt"), "p/a.txt");
        // A leading slash on the name is absorbed, never doubled.
        assert_eq!(join_key("p/", "/a.txt"), "p/a.txt");
    }

    #[test]
    fn folder_ids_always_end_in_a_slash() {
        assert_eq!(folder_id("", "docs"), "docs/");
        assert_eq!(folder_id("p/", "docs"), "p/docs/");
        assert_eq!(folder_id("p/", "docs/"), "p/docs/");
    }

    #[test]
    fn base_name_and_parent_walk_the_key() {
        assert_eq!(base_name("p/q/a.txt"), "a.txt");
        assert_eq!(base_name("p/q/"), "q");
        assert_eq!(base_name("a.txt"), "a.txt");
        assert_eq!(base_name(""), "");

        assert_eq!(parent_of("p/q/a.txt"), "p/q/");
        assert_eq!(parent_of("p/q/"), "p/");
        assert_eq!(parent_of("a.txt"), "");
        assert_eq!(parent_of(""), "");
    }

    #[test]
    fn props_round_trip_including_dots_and_non_ascii() {
        let mut props = HashMap::new();
        props.insert("driven.source_id".to_string(), "abc-123".to_string());
        props.insert("driven.client_op_uuid".to_string(), "uuid-1".to_string());
        // The trait permits arbitrary keys/values; a mangling scheme over
        // header names would lose these, base64 JSON does not.
        props.insert(
            "weird key/with.dots".to_string(),
            "vaLUE = \u{00e9}".to_string(),
        );

        let encoded = encode_props(&props)
            .unwrap()
            .expect("non-empty map encodes");
        // The header value must be header-safe ASCII.
        assert!(encoded.is_ascii());
        assert!(!encoded.contains(char::is_control));
        assert_eq!(decode_props(Some(&encoded)), props);
    }

    #[test]
    fn an_empty_map_writes_no_header() {
        assert_eq!(encode_props(&HashMap::new()).unwrap(), None);
        assert!(decode_props(None).is_empty());
    }

    #[test]
    fn corrupt_props_decode_to_empty_rather_than_failing_the_listing() {
        assert!(decode_props(Some("!!!not-base64!!!")).is_empty());
        let not_json = base64::engine::general_purpose::STANDARD.encode("hello");
        assert!(decode_props(Some(&not_json)).is_empty());
    }
}
