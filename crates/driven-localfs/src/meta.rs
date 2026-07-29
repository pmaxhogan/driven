//! The per-object metadata sidecar: how `app_properties` survive a round trip
//! through a plain filesystem.
//!
//! ## Why a sidecar at all
//!
//! [`RemoteStore`] hands every object a `HashMap<String, String>` of
//! `app_properties` and then SEARCHES on them (`find_by_op_uuid`,
//! `list_source_object_ids`). A filesystem has nowhere to put them. Three
//! options were on the table:
//!
//! - **Extended attributes.** The natural fit on APFS/ext4/NTFS - and
//!   completely absent on exFAT and FAT32, which are exactly the filesystems a
//!   USB backup stick is formatted with. A backend whose identity vocabulary
//!   evaporated on the most common removable format would be worse than no
//!   backend. Rejected.
//! - **One index file per destination** (or per source). One file to corrupt,
//!   one file to lock, and a guaranteed desync on any crash between writing an
//!   object and updating the index - the same argument `driven-s3` makes
//!   against a side-index. Rejected.
//! - **One sidecar per object**, which is what this module implements.
//!
//! ## Layout
//!
//! For a data object at `<dir>/<stored>` the sidecar is
//! `<dir>/.driven-meta/<stored>.json`. Deriving the sidecar path from the
//! object's own filename means:
//!
//! - a lookup by object id is a single `open`, no scan and no index;
//! - the sidecar namespace inherits the data namespace's uniqueness, which
//!   `crate::layout` has already made collision-free on the destination's own
//!   terms;
//! - `.driven-meta` keeps the metadata out of the user's line of sight, so the
//!   destination folder still looks like a plain mirror of their files - the
//!   whole point of a local backup you can browse and copy by hand.
//!
//! ## Commit ordering (load-bearing)
//!
//! **Data first, sidecar second, on write. Data first, sidecar second, on
//! delete.** In both directions the DATA file is the thing that exists or does
//! not, and the sidecar is the annotation.
//!
//! - Crash after the data rename, before the sidecar rename: an object with a
//!   stale or absent annotation. It is not lost, and the pending op replays and
//!   re-commits both. `find_by_op_uuid` correctly does not adopt it (no
//!   annotation, no uuid), so the replay re-creates it at exactly the same
//!   path - an overwrite, not a duplicate.
//! - Crash after the data delete, before the sidecar delete: a DANGLING
//!   sidecar. Harmless, and swept up by the next write to that name.
//!
//! The opposite ordering would be actively unsafe: a sidecar deleted first
//! leaves an ANNOTATION-LESS live data file, which
//! [`crate::store::LocalFsStore::list_source_object_ids`] cannot see - so the
//! remote-existence audit would call a live object dead and the file would be
//! re-uploaded beside it forever.
//!
//! For the same reason every reader in this crate is driven by the DATA file
//! and joins the sidecar onto it, never the other way around: a dangling
//! sidecar reported as live would tell the audit that a deleted file is still
//! on the drive, and it would never be re-uploaded.
//!
//! [`RemoteStore`]: driven_remote::remote_store::RemoteStore

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::names;

/// What a sidecar annotates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A data object.
    File,
    /// A directory Driven created via `ensure_folder`.
    Dir,
}

/// One object's metadata sidecar.
///
/// Field names are part of the stored format (v1.0.0 stability): an object
/// written by one Driven build must stay readable by the next. Add fields with
/// `#[serde(default)]`; never rename one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
    /// Stored-format version.
    pub version: u32,
    /// Whether this annotates a file or a directory.
    pub kind: EntryKind,
    /// The ORIGINAL name Driven was asked to store, before
    /// [`crate::names::encode`]. This is the field that makes case-insensitive
    /// collision detection possible: the encoded filename alone cannot tell
    /// `Foo.txt` from `foo.txt` on exFAT, but this can.
    pub name: String,
    /// The name actually used on the destination filesystem - the encoded name,
    /// possibly with a disambiguation tail.
    pub stored: String,
    /// Size in bytes of the data the sidecar was written for. Used to detect a
    /// sidecar that is out of step with its data file; see [`Sidecar::md5_for`].
    #[serde(default)]
    pub size: Option<u64>,
    /// Hex md5 of the data the sidecar was written for.
    #[serde(default)]
    pub md5: Option<String>,
    /// The object's MIME type, as the caller supplied it.
    #[serde(default)]
    pub mime: Option<String>,
    /// Unix epoch ms the object was written.
    #[serde(default)]
    pub modified_ms: i64,
    /// Driven's `app_properties` for the object, verbatim.
    #[serde(default)]
    pub props: HashMap<String, String>,
}

impl Sidecar {
    /// The md5 to report for a data file whose ACTUAL size is `actual_size`.
    ///
    /// `None` unless the recorded size matches, because a sidecar is committed
    /// AFTER its data file: a crash in that window leaves new bytes annotated
    /// with the previous version's digest, and reporting it would be worse than
    /// reporting nothing. A size mismatch catches that cheaply.
    ///
    /// A same-size change inside that window is not detectable this way and is
    /// the documented residual: nothing in the sync engine verifies against
    /// `metadata().md5` (uploads are verified against the digest the upload
    /// itself returns, and the deep-verify pass re-hashes locally), so the
    /// consequence is a stale display value until the pending op replays.
    pub fn md5_for(&self, actual_size: u64) -> Option<[u8; 16]> {
        if self.size != Some(actual_size) {
            return None;
        }
        let hex = self.md5.as_deref()?;
        let bytes = hex::decode(hex).ok()?;
        <[u8; 16]>::try_from(bytes.as_slice()).ok()
    }
}

/// The `.driven-meta` directory belonging to `dir`.
pub fn meta_dir(dir: &Path) -> PathBuf {
    dir.join(names::META_DIR)
}

/// The sidecar path for the object stored at `dir/stored`.
pub fn sidecar_path(dir: &Path, stored: &str) -> PathBuf {
    meta_dir(dir).join(format!("{stored}{}", names::META_EXT))
}

/// Read and parse a sidecar.
///
/// A missing sidecar is `None`. A CORRUPT sidecar is also `None`, with a
/// warning: the sidecar is Driven's own annotation, and an object whose
/// annotation cannot be read is correctly treated as "not annotated" by the
/// callers that search on it. Failing the whole listing instead would let one
/// truncated file wedge every audit on the drive.
pub fn read_sidecar(path: &Path) -> Option<Sidecar> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(
                target: crate::TARGET,
                path = %path.display(),
                %err,
                "could not read an object's metadata sidecar; treating the object as unannotated"
            );
            return None;
        }
    };
    match serde_json::from_slice::<Sidecar>(&raw) {
        Ok(s) => Some(s),
        Err(err) => {
            tracing::warn!(
                target: crate::TARGET,
                path = %path.display(),
                %err,
                "ignoring a metadata sidecar that is not valid JSON"
            );
            None
        }
    }
}

/// Write a sidecar durably (temp file, sync, atomic rename, directory sync).
pub fn write_sidecar(dir: &Path, sidecar: &Sidecar) -> std::io::Result<()> {
    let path = sidecar_path(dir, &sidecar.stored);
    let json = serde_json::to_vec(sidecar).map_err(std::io::Error::other)?;
    crate::fsx::write_durable(&path, &json)
}

/// Remove a sidecar; a missing one is success.
pub fn remove_sidecar(dir: &Path, stored: &str) -> std::io::Result<()> {
    crate::fsx::remove_if_present(&sidecar_path(dir, stored))
}

/// Every sidecar in `dir`'s `.driven-meta`, paired with the `stored` name its
/// filename implies.
///
/// Returns `Err` on a failure to ENUMERATE (as opposed to a failure to parse an
/// individual sidecar, which is skipped with a warning). Callers use this to
/// compute a complete set, and a silently short answer would read as "these
/// objects are gone".
pub fn list_sidecars(dir: &Path) -> std::io::Result<Vec<Sidecar>> {
    let meta = meta_dir(dir);
    let entries = match std::fs::read_dir(&meta) {
        Ok(e) => e,
        // No `.driven-meta` means no annotated objects here, which is a
        // complete and correct answer.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(names::META_EXT) || name.starts_with(names::TMP_PREFIX) {
            continue;
        }
        if let Some(s) = read_sidecar(&entry.path()) {
            out.push(s);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(stored: &str, name: &str) -> Sidecar {
        Sidecar {
            version: 1,
            kind: EntryKind::File,
            name: name.to_string(),
            stored: stored.to_string(),
            size: Some(3),
            md5: Some("00112233445566778899aabbccddeeff".to_string()),
            mime: Some("text/plain".to_string()),
            modified_ms: 42,
            props: HashMap::from([("driven.source_id".to_string(), "s1".to_string())]),
        }
    }

    #[test]
    fn sidecars_round_trip_through_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let s = sample("a.txt", "a.txt");
        write_sidecar(dir.path(), &s).unwrap();
        let back = read_sidecar(&sidecar_path(dir.path(), "a.txt")).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn props_survive_dots_and_non_ascii_which_xattr_keys_would_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = sample("b.bin", "b.bin");
        s.props.insert(
            "weird key/with.dots".to_string(),
            "vaLUE = \u{00e9} \u{1f600}".to_string(),
        );
        write_sidecar(dir.path(), &s).unwrap();
        assert_eq!(read_sidecar(&sidecar_path(dir.path(), "b.bin")).unwrap(), s);
    }

    #[test]
    fn a_corrupt_sidecar_reads_as_unannotated_rather_than_failing_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(meta_dir(dir.path())).unwrap();
        std::fs::write(sidecar_path(dir.path(), "x.txt"), b"{ truncated").unwrap();
        assert!(read_sidecar(&sidecar_path(dir.path(), "x.txt")).is_none());
        // And it does not stop the enumeration of its healthy neighbours.
        write_sidecar(dir.path(), &sample("y.txt", "y.txt")).unwrap();
        let all = list_sidecars(dir.path()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].stored, "y.txt");
    }

    #[test]
    fn md5_is_withheld_when_the_sidecar_is_out_of_step_with_its_data() {
        let s = sample("a.txt", "a.txt");
        assert!(s.md5_for(3).is_some(), "matching size reports the digest");
        assert!(
            s.md5_for(4).is_none(),
            "a size mismatch means the sidecar predates the data; reporting its \
             digest would be a lie the executor could act on"
        );
    }

    #[test]
    fn listing_an_absent_meta_dir_is_an_empty_complete_answer() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_sidecars(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn removing_a_sidecar_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        remove_sidecar(dir.path(), "nope.txt").unwrap();
        write_sidecar(dir.path(), &sample("c.txt", "c.txt")).unwrap();
        remove_sidecar(dir.path(), "c.txt").unwrap();
        remove_sidecar(dir.path(), "c.txt").unwrap();
        assert!(read_sidecar(&sidecar_path(dir.path(), "c.txt")).is_none());
    }
}
