//! The `app_properties` vocabulary shared by every backend (SPEC s3 preamble).
//!
//! These keys are Driven's canonical identity for the objects it owns. They are
//! declared ONCE, here, because the producer and the consumer of each key live
//! in different crates:
//!
//! - the executor (`driven-core`) STAMPS them on create/update;
//! - each backend (`driven-drive`, `driven-s3`, ...) SEARCHES on them in
//!   `find_by_op_uuid` and `list_source_object_ids`.
//!
//! A second copy that drifted would make the audit query match NOTHING, read
//! every recorded id as dead, and re-upload the entire source. So there is
//! exactly one definition per key and everyone re-exports it.

/// `app_properties` key marking folders Driven created (SPEC s3
/// `ensure_folder` disambiguation).
pub const FOLDER_MARKER_KEY: &str = "driven.folder_marker";

/// `app_properties` key carrying the crash-safe create-op UUID (DESIGN s5.6).
pub const CLIENT_OP_UUID_KEY: &str = "driven.client_op_uuid";

/// `app_properties` key carrying the id of the source an object belongs to
/// (SPEC s3 preamble). Stamped by the executor on every object it creates -
/// per-file objects and `.tar.gz` bundles alike - and queried back by
/// [`crate::remote_store::RemoteStore::list_source_object_ids`] to enumerate a
/// source's live footprint.
pub const SOURCE_ID_KEY: &str = "driven.source_id";

/// `app_properties` key carrying the relative-path hash (SPEC s3 preamble).
/// Stamped by the executor; no backend searches on it today, but it is part of
/// the same identity vocabulary and belongs beside its siblings.
pub const RELATIVE_PATH_HASH_KEY: &str = "driven.relative_path_hash";

/// `app_properties` key stamped on a `.tar.gz` bundle object marking it as a
/// Driven bundle and naming its archive format (V2 small-file bundling, issue
/// #35).
pub const BUNDLE_FORMAT_KEY: &str = "driven.bundle_format";
