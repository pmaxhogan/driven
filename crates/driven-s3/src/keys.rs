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
use md5::{Digest, Md5};

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

/// The folder segment holding Driven's OWN version store (issue #220).
///
/// A versioned change on S3 would otherwise re-`PutObject` over the same key and
/// destroy the bytes the retained `file_versions` row points at, because the
/// create key is a pure function of the name. `S3Store::archive_version` instead
/// server-side-copies the superseded object under this prefix first, and the
/// version row points THERE.
///
/// Part of the stored format: renaming it would orphan every archived version
/// written by an earlier build. The leading dot keeps it out of the way in a
/// bucket the user also browses, and it deliberately mirrors
/// `driven-localfs`'s `.driven-versions` directory so the two destinations read
/// the same when a user looks at them.
pub const VERSIONS_SEGMENT: &str = ".driven-versions";

/// S3's hard limit on an object key, in bytes.
const MAX_KEY_LEN: usize = 1024;

/// The slash-terminated prefix of Driven's version store under `root_prefix`.
pub fn versions_prefix(root_prefix: &str) -> String {
    folder_id(root_prefix, VERSIONS_SEGMENT)
}

/// Whether `key` names an object in Driven's version store.
///
/// Used to keep archived versions out of the folder picker and out of the
/// remote-existence audit's live set, and - load-bearing - to make `trash` a
/// no-op for them: on S3 `trash` is a permanent `DeleteObject`, so trashing a
/// superseded object the way the Drive path does would destroy the very version
/// that was just recorded.
pub fn is_version_key(root_prefix: &str, key: &str) -> bool {
    key.starts_with(&versions_prefix(root_prefix))
}

/// The key an archived copy of `live_key` holding content `content_token` lives
/// at.
///
/// DETERMINISTIC in `(live_key, content_token)`, which is what makes archiving
/// idempotent: an op that crashes after the archive but before its commit is
/// replayed against the same token and lands on the same key, so a replay can
/// neither accumulate copies nor (because the caller skips an archive that
/// already exists) overwrite a correct archive with the newer content.
///
/// The readable form keeps the original path visible - a user browsing the
/// bucket sees `.driven-versions/notes/todo.txt@<token>` beside their backup
/// rather than an opaque digest. The prefix and the token add a fixed ~80 bytes
/// (the token is a full hex BLAKE3), so a live key within that of S3's
/// 1024-byte limit would breach it; those fall back to a digest of the relative
/// key, which fits any input while staying just as deterministic.
pub fn version_key(root_prefix: &str, live_key: &str, content_token: &str) -> String {
    let prefix = versions_prefix(root_prefix);
    let relative = live_key.strip_prefix(root_prefix).unwrap_or(live_key);
    let readable = format!("{prefix}{relative}@{content_token}");
    if readable.len() <= MAX_KEY_LEN {
        return readable;
    }
    let mut hasher = Md5::new();
    hasher.update(relative.as_bytes());
    let digest: [u8; 16] = hasher.finalize().into();
    format!("{prefix}{}@{content_token}", hex::encode(digest))
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
    fn version_keys_are_deterministic_and_live_under_the_version_store() {
        // Same inputs, same key: this is what makes archiving idempotent across
        // a crash + replay (issue #220).
        let a = version_key("root/", "root/docs/a.txt", "deadbeef");
        assert_eq!(a, version_key("root/", "root/docs/a.txt", "deadbeef"));
        assert_eq!(a, "root/.driven-versions/docs/a.txt@deadbeef");
        assert!(is_version_key("root/", &a));
        // Different content is a DIFFERENT object; that is the whole point.
        assert_ne!(a, version_key("root/", "root/docs/a.txt", "cafe0000"));
        // ... and so is a different file with identical content, so pruning one
        // file's version can never delete another file's.
        assert_ne!(a, version_key("root/", "root/docs/b.txt", "deadbeef"));

        // At the bucket root the relative key IS the key.
        let r = version_key("", "docs/a.txt", "0123");
        assert_eq!(r, ".driven-versions/docs/a.txt@0123");
        assert!(is_version_key("", &r));

        // A live object is never mistaken for an archived one.
        assert!(!is_version_key("root/", "root/docs/a.txt"));
        assert!(!is_version_key("", "docs/a.txt"));
        // An object under ANOTHER prefix's version store is not ours.
        assert!(!is_version_key("root/", "other/.driven-versions/a.txt@1"));
    }

    #[test]
    fn an_over_long_version_key_falls_back_to_a_digest_but_stays_deterministic() {
        // S3 refuses a key over 1024 bytes, so the readable form cannot be used
        // for a live key already close to the limit.
        let long = format!("root/{}", "x".repeat(1_010));
        let key = version_key("root/", &long, "deadbeefdeadbeef");
        assert!(
            key.len() <= 1_024,
            "an archive key must fit S3's limit, got {}",
            key.len()
        );
        assert!(is_version_key("root/", &key));
        assert_eq!(key, version_key("root/", &long, "deadbeefdeadbeef"));
        // Still one archive per (file, content).
        assert_ne!(key, version_key("root/", &long, "0000000000000000"));
        let other = format!("root/{}", "y".repeat(1_010));
        assert_ne!(key, version_key("root/", &other, "deadbeefdeadbeef"));
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
