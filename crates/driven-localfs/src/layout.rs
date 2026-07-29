//! Object ids, their filesystem paths, and the collision-safe name claim.
//!
//! ## Ids
//!
//! An id is a `/`-separated path RELATIVE to the destination root, built from
//! [`crate::names::encode`]d components. `""` is the root; a FOLDER id ends in
//! `/`; a FILE id does not:
//!
//! ```text
//! root folder id           = ""
//! ensure_folder("", "Docs") = "Docs/"
//! create("Docs/", "a.txt")  = "Docs/a.txt"
//! ```
//!
//! This is the same prefix-as-folder-id shape `driven-s3` uses, for the same
//! reason: [`RemoteStore`] is folder-shaped, and an id that is derived from the
//! path is stable across restarts and readable in a log line.
//!
//! Unlike Drive (where an id is an opaque handle stable across renames) the id
//! IS the location, so a rename is a delete plus a create - exactly what the
//! executor already plans, since it keys `file_state` by relative path.
//!
//! ## Case-insensitive collisions
//!
//! exFAT, FAT32 and a default-configured APFS volume are case-INSENSITIVE.
//! `Notes/Foo.txt` and `Notes/foo.txt` are two distinct source files that both
//! want the same destination filename, and the naive answer - just write it -
//! means the second silently destroys the first and every later backup reports
//! success while one of the two files does not exist on the drive.
//!
//! [`resolve_stored_name`] refuses that. The probe is deliberately performed by
//! ASKING THE DESTINATION - opening `<dir>/.driven-meta/<encoded>.json` and
//! reading the ORIGINAL name recorded there - which means it inherits the
//! destination filesystem's own equivalence relation for free. Case folding,
//! Unicode normalization (an NFD `e` + combining acute versus a precomposed NFC
//! `\u{e9}`, which HFS+/APFS fold and ext4 does not), and any locale-specific
//! folding the driver applies are all handled without Driven shipping a single
//! table, because the filesystem itself resolves the open. It therefore detects
//! exactly the collisions that filesystem would actually have caused - no more
//! and no fewer.
//!
//! On collision the name gets a deterministic `~<digest>` tail
//! ([`crate::names::disambiguate`]), so both files land on the drive and the
//! same source file always lands on the same destination file.
//!
//! ## Concurrency
//!
//! The executor uploads several files from one source in parallel, so two
//! colliding names can be in flight at once - and the probe above cannot see a
//! sidecar that has not been committed yet. [`NameClaims`] closes that: the
//! probe and the claim happen together under one lock, and an in-flight claim
//! counts as an owner exactly like a committed sidecar does.
//!
//! **What this does NOT cover:** another PROCESS writing into the same
//! destination folder - a second Driven instance, or the user's own file
//! manager. Driven runs single-instance, and a destination folder is Driven's
//! to manage, so the in-process guarantee is the whole guarantee. A concurrent
//! external writer can still lose a race; no filesystem primitive that works on
//! FAT32 would fix that (there are no directory locks, and `O_EXCL` alone
//! cannot distinguish "someone else's live claim" from "an orphan left by a
//! crash").
//!
//! [`RemoteStore`]: driven_remote::remote_store::RemoteStore

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::meta::{self, EntryKind};
use crate::names;

/// Normalize a caller-supplied folder id to the canonical slash-terminated
/// form (`""` stays `""`).
pub fn folder_prefix(folder_id: &str) -> String {
    if folder_id.is_empty() || folder_id.ends_with('/') {
        folder_id.to_string()
    } else {
        format!("{folder_id}/")
    }
}

/// The id of `name` (already encoded) under the folder `parent_id`.
pub fn join_id(parent_id: &str, stored: &str) -> String {
    format!("{}{}", folder_prefix(parent_id), stored)
}

/// The folder id for `stored` under `parent_id`.
pub fn folder_id(parent_id: &str, stored: &str) -> String {
    format!("{}/", join_id(parent_id, stored.trim_end_matches('/')))
}

/// The last component of an id (`""` for the root).
pub fn base_name(id: &str) -> &str {
    id.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// The parent folder id of an id (`""` at the root).
pub fn parent_of(id: &str) -> String {
    let trimmed = id.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[..=i].to_string(),
        None => String::new(),
    }
}

/// Resolve an id to an absolute path under `root`, refusing anything that could
/// escape the destination.
///
/// Ids are BUILT from encoded components, which can never contain `/`, `\`, a
/// NUL, or be `.`/`..` - but an id also arrives from `file_state`, i.e. from
/// SQLite, i.e. from a file on disk a user could edit. So the invariant is
/// re-checked here rather than assumed: a hand-edited `../../etc` must not let
/// a "backup" write outside the destination folder.
pub fn path_for_id(root: &Path, id: &str) -> anyhow::Result<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in id.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.contains('\\') || segment.contains('\0') {
            anyhow::bail!("localfs.id_invalid: object id {id:?} escapes the destination folder");
        }
        out.push(segment);
    }
    Ok(out)
}

/// Should this directory entry be hidden from listings and audits?
///
/// Two families qualify. Driven's own control files - the sidecar directory,
/// in-progress temp files and the destination marker - are infrastructure, not
/// backed-up objects. And on exFAT/FAT32, macOS writes an AppleDouble `._X`
/// shadow beside every `X` to hold the extended attributes the filesystem
/// cannot store natively (see [`crate::names::APPLEDOUBLE_PREFIX`]).
///
/// Surfacing either would put `.driven-meta` in the destination picker, double
/// every listing on a USB stick, and make the remote-existence audit try to heal
/// a temp file forever.
pub fn is_control_entry(name: &str) -> bool {
    names::is_reserved_control_name(name)
}

/// The set of destination names currently being written, so a probe can see a
/// claim that has not been committed to a sidecar yet.
///
/// Keyed by `(folder id, ASCII-lowercased stored name)`. The lowercasing is a
/// cheap approximation of the destination's folding used ONLY to decide which
/// claims to compare; the authoritative answer still comes from the filesystem
/// probe, so a folding this misses (a Unicode-normalizing volume, say) degrades
/// to the same window an external writer has, not to a wrong answer.
#[derive(Default)]
pub struct NameClaims {
    inner: Mutex<HashMap<(String, String), (String, usize)>>,
}

/// A live claim on a destination name. Released on drop.
pub struct ClaimGuard {
    claims: Arc<NameClaims>,
    key: (String, String),
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let mut map = self.claims.inner.lock();
        if let Some((_, count)) = map.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// Where a candidate destination name stands.
#[derive(Debug, PartialEq, Eq)]
enum Ownership {
    /// Free, or already ours.
    Ours,
    /// Held by a different original name; try the next candidate.
    Taken,
}

/// Choose the destination filename for `original` inside `dir`, claiming it for
/// the duration of the returned guard.
///
/// `dir_id` is the folder id (for the claim key), `dir_path` its absolute path.
/// `kind` distinguishes a file from a directory: the two share one filesystem
/// namespace - a directory and a file cannot both be called `Reports` in the
/// same folder, even though Drive allows exactly that - so a file/directory
/// clash is treated as a collision and disambiguated, not as an error.
pub fn resolve_stored_name(
    claims: &Arc<NameClaims>,
    dir_path: &Path,
    dir_id: &str,
    original: &str,
    kind: EntryKind,
) -> anyhow::Result<(String, ClaimGuard)> {
    let encoded = names::encode(original)?;
    let candidates = [encoded.clone(), names::disambiguate(&encoded, original)];

    // Held across the (short, local, blocking) sidecar reads so the probe and
    // the claim are one atomic step. Never held across an await: every caller
    // runs this inside a `spawn_blocking`.
    let mut map = claims.inner.lock();

    for candidate in candidates {
        let key = (dir_id.to_string(), candidate.to_ascii_lowercase());

        // An in-flight claim by a DIFFERENT original owns the name even though
        // no sidecar exists yet.
        if let Some((holder, _)) = map.get(&key) {
            if holder != original {
                continue;
            }
        } else if ownership(dir_path, &candidate, original, kind) == Ownership::Taken {
            continue;
        }

        let entry = map
            .entry(key.clone())
            .or_insert_with(|| (original.to_string(), 0));
        entry.1 += 1;
        return Ok((
            candidate,
            ClaimGuard {
                claims: Arc::clone(claims),
                key,
            },
        ));
    }

    // Both candidates are held by other names. Reaching here needs a collision
    // in the 64-bit disambiguation digest; failing loudly beats overwriting
    // someone else's file.
    anyhow::bail!(
        "localfs.name_collision: could not find a free destination filename for {original:?} \
         in {}; both the encoded name and its disambiguated form are taken",
        dir_path.display()
    )
}

/// Claim a destination name that has ALREADY been decided, without probing.
///
/// Two callers need this: an `update`, which owns its target by definition
/// (the caller carried the id), and the hydration of a resumable session opened
/// by a previous process, whose chosen name is recorded in the session handle.
/// Re-probing in either case would be wrong - the sidecar is not committed yet,
/// so the probe could hand the name to someone else.
pub fn claim_exact(
    claims: &Arc<NameClaims>,
    dir_id: &str,
    stored: &str,
    original: &str,
) -> ClaimGuard {
    let key = (dir_id.to_string(), stored.to_ascii_lowercase());
    let mut map = claims.inner.lock();
    let entry = map
        .entry(key.clone())
        .or_insert_with(|| (original.to_string(), 0));
    entry.1 += 1;
    ClaimGuard {
        claims: Arc::clone(claims),
        key,
    }
}

/// Does `candidate` in `dir_path` already belong to a DIFFERENT original name?
fn ownership(dir_path: &Path, candidate: &str, original: &str, kind: EntryKind) -> Ownership {
    match meta::read_sidecar(&meta::sidecar_path(dir_path, candidate)) {
        Some(s) if s.name == original && s.kind == kind => Ownership::Ours,
        // A different original name, or the same name as the other KIND (a file
        // where we want a directory). Either way the destination filesystem
        // cannot hold both.
        Some(_) => Ownership::Taken,
        None => {
            // No sidecar. If something is nonetheless sitting at that path it is
            // unowned: a data file left by a create that crashed before its
            // sidecar was committed, or a file the user put in the destination
            // folder themselves. Taking it is correct for the first case (the
            // replay must land on the same path) and is the only non-wedging
            // answer for the second, but it is worth a log line.
            let data = dir_path.join(candidate);
            if data.symlink_metadata().is_ok() {
                tracing::warn!(
                    target: crate::TARGET,
                    path = %data.display(),
                    "overwriting an unannotated file already at this destination path \
                     (a crashed upload, or a file Driven did not put there)"
                );
            }
            Ownership::Ours
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Arc<NameClaims> {
        Arc::new(NameClaims::default())
    }

    #[test]
    fn ids_join_and_walk() {
        assert_eq!(join_id("", "a.txt"), "a.txt");
        assert_eq!(join_id("d/", "a.txt"), "d/a.txt");
        assert_eq!(join_id("d", "a.txt"), "d/a.txt");
        assert_eq!(folder_id("", "Docs"), "Docs/");
        assert_eq!(folder_id("d/", "Docs/"), "d/Docs/");
        assert_eq!(base_name("d/e/a.txt"), "a.txt");
        assert_eq!(base_name("d/e/"), "e");
        assert_eq!(base_name(""), "");
        assert_eq!(parent_of("d/e/a.txt"), "d/e/");
        assert_eq!(parent_of("a.txt"), "");
        assert_eq!(parent_of(""), "");
    }

    #[test]
    fn an_id_can_never_escape_the_destination_folder() {
        let root = Path::new("/dest");
        assert_eq!(
            path_for_id(root, "a/b.txt").unwrap(),
            PathBuf::from("/dest/a/b.txt")
        );
        assert_eq!(path_for_id(root, "").unwrap(), PathBuf::from("/dest"));
        for evil in ["../etc/passwd", "a/../../etc", "a/./b", "a\\b", "a\0b"] {
            assert!(
                path_for_id(root, evil).is_err(),
                "{evil:?} must be refused, not resolved"
            );
        }
    }

    #[test]
    fn a_fresh_name_is_stored_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let c = claims();
        let (stored, _g) =
            resolve_stored_name(&c, dir.path(), "", "report.txt", EntryKind::File).unwrap();
        assert_eq!(stored, "report.txt");
    }

    #[test]
    fn a_committed_sidecar_for_the_same_name_is_reused_so_updates_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let c = claims();
        let (stored, guard) =
            resolve_stored_name(&c, dir.path(), "", "Foo.txt", EntryKind::File).unwrap();
        meta::write_sidecar(
            dir.path(),
            &meta::Sidecar {
                version: 1,
                kind: EntryKind::File,
                name: "Foo.txt".to_string(),
                stored: stored.clone(),
                size: Some(0),
                md5: None,
                mime: None,
                modified_ms: 0,
                props: HashMap::new(),
            },
        )
        .unwrap();
        drop(guard);

        let (again, _g) =
            resolve_stored_name(&c, dir.path(), "", "Foo.txt", EntryKind::File).unwrap();
        assert_eq!(again, stored, "an update must land on the same file");
    }

    #[test]
    fn a_case_only_difference_gets_its_own_file_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let c = claims();
        let (first, guard) =
            resolve_stored_name(&c, dir.path(), "", "Foo.txt", EntryKind::File).unwrap();
        meta::write_sidecar(
            dir.path(),
            &meta::Sidecar {
                version: 1,
                kind: EntryKind::File,
                name: "Foo.txt".to_string(),
                stored: first.clone(),
                size: Some(0),
                md5: None,
                mime: None,
                modified_ms: 0,
                props: HashMap::new(),
            },
        )
        .unwrap();
        drop(guard);

        // On a case-INSENSITIVE volume the sidecar open resolves to `Foo.txt`'s
        // and the recorded original name gives the collision away. On a
        // case-SENSITIVE one there is no collision to detect and `foo.txt` is
        // stored verbatim - which is also correct, because the filesystem can
        // hold both.
        let (second, _g) =
            resolve_stored_name(&c, dir.path(), "", "foo.txt", EntryKind::File).unwrap();
        assert_ne!(
            second, first,
            "two distinct source names must never share one destination file"
        );
        if second != "foo.txt" {
            assert!(second.starts_with("foo~"), "{second}");
            assert!(second.ends_with(".txt"), "{second}");
            assert_eq!(
                second,
                names::disambiguate("foo.txt", "foo.txt"),
                "disambiguation must be deterministic"
            );
        }
    }

    #[test]
    fn an_in_flight_claim_blocks_a_colliding_name_before_any_sidecar_exists() {
        // The parallel-upload race: two files whose names differ only by case
        // are uploaded at once, and neither has committed a sidecar yet.
        let dir = tempfile::tempdir().unwrap();
        let c = claims();
        let (a, _guard_a) =
            resolve_stored_name(&c, dir.path(), "", "Foo.txt", EntryKind::File).unwrap();
        let (b, _guard_b) =
            resolve_stored_name(&c, dir.path(), "", "FOO.txt", EntryKind::File).unwrap();
        assert_ne!(a, b, "concurrent colliding claims must not share a file");
    }

    #[test]
    fn a_claim_is_released_when_its_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let c = claims();
        {
            let (_a, _g) =
                resolve_stored_name(&c, dir.path(), "", "Foo.txt", EntryKind::File).unwrap();
        }
        // With the claim released and no sidecar committed, a different name may
        // now take the slot (the previous upload failed, so nothing owns it).
        let (b, _g) = resolve_stored_name(&c, dir.path(), "", "foo.txt", EntryKind::File).unwrap();
        assert_eq!(b, "foo.txt");
    }

    #[test]
    fn a_file_and_a_directory_cannot_share_one_destination_name() {
        // Drive allows a file and a folder with the same name in one parent; no
        // filesystem does. The clash must disambiguate rather than collide.
        let dir = tempfile::tempdir().unwrap();
        let c = claims();
        let (f, guard) =
            resolve_stored_name(&c, dir.path(), "", "Reports", EntryKind::File).unwrap();
        meta::write_sidecar(
            dir.path(),
            &meta::Sidecar {
                version: 1,
                kind: EntryKind::File,
                name: "Reports".to_string(),
                stored: f.clone(),
                size: Some(0),
                md5: None,
                mime: None,
                modified_ms: 0,
                props: HashMap::new(),
            },
        )
        .unwrap();
        drop(guard);

        let (d, _g) = resolve_stored_name(&c, dir.path(), "", "Reports", EntryKind::Dir).unwrap();
        assert_ne!(d, f);
    }

    #[test]
    fn driven_control_names_are_hidden_from_listings() {
        assert!(is_control_entry(names::META_DIR));
        assert!(is_control_entry(names::MARKER_FILE));
        assert!(is_control_entry(".driven-tmp-abc"));
        assert!(!is_control_entry("report.txt"));
    }
}
