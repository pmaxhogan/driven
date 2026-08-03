//! The per-object metadata sidecar: how `app_properties` survive a round trip
//! through SFTP.
//!
//! ## Why a sidecar at all
//!
//! [`RemoteStore`] hands every object a `HashMap<String, String>` of
//! `app_properties` and then SEARCHES on them (`find_by_op_uuid`,
//! `list_source_object_ids`). SFTP has no user-metadata headers - it is a
//! filesystem protocol, and SFTPv3 `SSH_FXP_EXTENDED` attributes are neither
//! universally supported nor durable across servers. The options were the same
//! three `driven-localfs` faced:
//!
//! - **Remote extended attributes.** Not exposed by SFTPv3 at all, and the
//!   `xattr@openssh.com` shape is not something every server (Synology, QNAP,
//!   `internal-sftp` on an older box) offers. Rejected.
//! - **One index file per destination.** One file to corrupt, one file to lock,
//!   and a guaranteed desync on any crash between writing an object and
//!   updating the index - and over a network the crash window is much wider.
//!   Rejected.
//! - **One sidecar per object**, which is what this module implements.
//!
//! ## Layout
//!
//! For a data object at `<dir>/<stored>` the sidecar is
//! `<dir>/.<stored>.driven-meta`, a SIBLING of the object rather than a file
//! inside a metadata directory (which is what `driven-localfs` does). The
//! difference is deliberate: over the wire, a sidecar read inside a
//! subdirectory would cost an extra `mkdir`/`stat` per directory, and the whole
//! point of deriving the sidecar name from the object's own name is that a
//! lookup by object id is a SINGLE round trip with no scan and no index.
//!
//! The cost of the flat layout is that sidecars and objects share one
//! namespace, so `.driven-meta` becomes a reserved SUFFIX -
//! [`crate::names::encode`] guarantees no object name can end with it, and
//! `list_folder` filters it. Both halves are required: without the escape a
//! user's own `notes.driven-meta` would be invisible forever.
//!
//! ## Commit ordering (load-bearing)
//!
//! **Data first, sidecar second, on write. Data first, sidecar second, on
//! delete.** In both directions the DATA file is the thing that exists or does
//! not, and the sidecar is the annotation.
//!
//! - Crash after the data rename, before the sidecar write: an object with a
//!   stale or absent annotation. It is not lost, and the pending op replays and
//!   re-commits both. `find_by_op_uuid` correctly does not adopt it (no
//!   annotation, no uuid), so the replay re-creates it at exactly the same path -
//!   an overwrite, not a duplicate.
//! - Crash after the data delete, before the sidecar delete: a DANGLING
//!   sidecar. Harmless, and swept up by the next write to that name.
//!
//! The opposite ordering would be actively unsafe: a sidecar deleted first
//! leaves an ANNOTATION-LESS live data file, which the remote-existence audit
//! cannot see - so it would call a live object dead and the file would be
//! re-uploaded beside itself forever.
//!
//! For the same reason every reader in this crate is driven by the DATA file
//! and joins the sidecar onto it, never the other way around.
//!
//! [`RemoteStore`]: driven_remote::remote_store::RemoteStore

use std::collections::HashMap;

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
/// written by one Driven build must stay readable by the next, and - because
/// the encoding is byte-identical to the local-folder backend's - a tree
/// `rsync`ed between an SFTP server and a USB stick must stay readable too. Add
/// fields with `#[serde(default)]`; never rename one.
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
    /// `Foo.txt` from `foo.txt` on a Synology or Windows share, but this can.
    pub name: String,
    /// The name actually used on the remote - the encoded name, possibly with a
    /// disambiguation tail.
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
    /// itself re-reads off the remote, and the deep-verify pass re-hashes
    /// locally), so the consequence is a stale display value until the pending
    /// op replays.
    pub fn md5_for(&self, actual_size: u64) -> Option<[u8; 16]> {
        if self.size != Some(actual_size) {
            return None;
        }
        let hex = self.md5.as_deref()?;
        let bytes = hex::decode(hex).ok()?;
        <[u8; 16]>::try_from(bytes.as_slice()).ok()
    }

    /// Serialize for the wire.
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self)
            .map_err(|e| anyhow::anyhow!("sftp.sidecar_invalid: could not encode a sidecar: {e}"))
    }
}

/// Is `stored` a SINGLE safe path component - the only shape a sidecar filename
/// may be built from?
///
/// [`crate::names::encode`] escapes `/`, `\` and the whole-name `.`/`..`, so a
/// legitimate `stored` already is one - but `stored` also arrives from
/// `file_state.drive_file_id`, i.e. from SQLite, i.e. from a file on disk a
/// user could edit. Every sidecar read, write and delete refuses outright
/// rather than sanitizing: a name that is not a plain component is a corrupt
/// row, and quietly rewriting it to something else would annotate the WRONG
/// object.
fn is_safe_stored_component(stored: &str) -> bool {
    !stored.is_empty()
        && stored != "."
        && !stored.contains("..")
        && !stored.contains('/')
        && !stored.contains('\\')
        && !stored.contains('\0')
}

/// The sidecar filename for the object stored as `stored`, or `None` when
/// `stored` is not a single safe path component.
///
/// The guard is an early return, not a transform, so a rejected name never
/// reaches an SFTP request at all.
pub fn sidecar_name(stored: &str) -> Option<String> {
    if !is_safe_stored_component(stored) {
        tracing::warn!(
            target: crate::TARGET,
            %stored,
            "refusing a sidecar name that is not a single path component"
        );
        return None;
    }
    Some(format!(".{stored}{}", names::META_SUFFIX))
}

/// The `stored` name a sidecar filename annotates, or `None` when `file_name`
/// is not a sidecar.
///
/// Exactly the inverse of [`sidecar_name`], so a directory walk can pair every
/// sidecar back onto the object it belongs to without a second request.
pub fn stored_from_sidecar_name(file_name: &str) -> Option<&str> {
    let stored = file_name
        .strip_prefix('.')?
        .strip_suffix(names::META_SUFFIX)?;
    (!stored.is_empty()).then_some(stored)
}

/// Parse sidecar bytes read off the remote.
///
/// A CORRUPT sidecar is `None`, with a warning: the sidecar is Driven's own
/// annotation, and an object whose annotation cannot be read is correctly
/// treated as "not annotated" by the callers that search on it. Failing the
/// whole listing instead would let one truncated file wedge every audit against
/// the server.
pub fn parse(context: &str, raw: &[u8]) -> Option<Sidecar> {
    match serde_json::from_slice::<Sidecar>(raw) {
        Ok(s) => Some(s),
        Err(err) => {
            tracing::warn!(
                target: crate::TARGET,
                %context,
                %err,
                "ignoring a metadata sidecar that is not valid JSON"
            );
            None
        }
    }
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
    fn sidecars_round_trip_through_json() {
        let s = sample("a.txt", "a.txt");
        let back = parse("test", &s.to_bytes().unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn props_survive_dots_and_non_ascii_which_an_xattr_key_would_not() {
        let mut s = sample("b.bin", "b.bin");
        s.props.insert(
            "weird key/with.dots".to_string(),
            "vaLUE = \u{00e9} \u{1f600}".to_string(),
        );
        assert_eq!(parse("test", &s.to_bytes().unwrap()).unwrap(), s);
    }

    #[test]
    fn a_corrupt_sidecar_reads_as_unannotated_rather_than_failing() {
        assert!(parse("test", b"{ truncated").is_none());
        assert!(parse("test", b"").is_none());
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
    fn a_sidecar_name_is_the_object_name_with_a_dot_and_the_suffix() {
        assert_eq!(
            sidecar_name("report.txt").as_deref(),
            Some(".report.txt.driven-meta")
        );
        // A dotfile object still gets an unambiguous, invertible sidecar name.
        assert_eq!(sidecar_name(".env").as_deref(), Some("..env.driven-meta"));
    }

    #[test]
    fn sidecar_names_invert_exactly() {
        for stored in ["report.txt", ".env", "a%3Ab", "no-extension", "x"] {
            let name = sidecar_name(stored).expect("a plain component is nameable");
            assert_eq!(stored_from_sidecar_name(&name), Some(stored), "{stored}");
            assert!(
                names::is_reserved_control_name(&name),
                "{name} must be filtered out of every listing"
            );
        }
        // Anything that is not a sidecar does not decode as one.
        for other in ["report.txt", ".driven-tmp-1", ".driven-meta", "", "."] {
            assert_eq!(stored_from_sidecar_name(other), None, "{other:?}");
        }
    }

    #[test]
    fn a_sidecar_name_that_is_not_one_component_is_refused_outright() {
        // `names::encode` cannot produce any of these, but `stored` also comes
        // from `file_state.drive_file_id` - a row in a file a user can edit.
        for evil in [
            "../../etc/passwd",
            "..",
            ".",
            "a/b",
            "a\\b",
            "",
            "/etc/passwd",
            "a\0b",
        ] {
            assert!(
                sidecar_name(evil).is_none(),
                "{evil:?} must be refused, not resolved"
            );
        }
    }
}
