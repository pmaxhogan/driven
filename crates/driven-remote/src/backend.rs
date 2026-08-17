//! The backup-destination taxonomy: which kind of [`RemoteStore`] an account
//! talks to.
//!
//! [`RemoteStore`]: crate::remote_store::RemoteStore
//!
//! Driven shipped with exactly one destination (Google Drive), so the choice
//! was implicit everywhere: `assembly::build_remote` constructed a
//! `GoogleDriveStore`, and so did the folder picker and the restore path. This
//! module makes the choice EXPLICIT and per-account, so a second backend is a
//! new enum variant plus a factory arm rather than a fork of every construction
//! site.
//!
//! ## Stored format (v1.0.0 stability)
//!
//! The kind is persisted as the nullable `accounts.backend_kind` TEXT column
//! (migration `0013`). Every pre-migration row is `NULL`, which
//! [`BackendKind::from_stored`] decodes to [`BackendKind::GoogleDrive`] - so
//! existing accounts keep working with no data migration and no re-auth. The
//! empty string decodes the same way (a defensive equivalence, mirroring how
//! `DriveContext::from_stored` treats `NULL`/`""`/`"my-drive"` alike).
//!
//! An UNRECOGNISED value is an ERROR, not a fallback. A config written by a
//! NEWER Driven naming a backend this build does not know must fail loudly: a
//! silent fallback to Drive would point an account's uploads at the wrong
//! destination entirely.

use std::fmt;

/// Which backup destination an account is configured against.
///
/// The wire/stored representation is the lowercase snake-case
/// [`BackendKind::id`] string; do not use `Debug` or `Display` output as a
/// storage key.
/// The serde representation is the snake_case variant name, which is BY
/// CONSTRUCTION the same string as [`BackendKind::id`] - `ids_match_serde`
/// below pins that equivalence so the DB column, the IPC payload and the
/// `serde` encoding can never drift apart.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// Google Drive over the Drive v3 REST API, authorized by the SPEC s4 PKCE
    /// loopback OAuth flow. The historical default and the value every
    /// pre-migration account decodes to.
    #[default]
    GoogleDrive,
    /// Any S3-compatible object store (AWS S3, Cloudflare R2, MinIO, Backblaze
    /// B2, Wasabi, Ceph RGW, ...), authorized by a directly-entered access key
    /// pair rather than an OAuth consent flow.
    S3,
    /// A plain directory on this machine: a USB drive, an external SSD, a NAS
    /// mount, or any local folder. Needs no credential at all - the user
    /// already has write access to the folder they chose.
    LocalFolder,
    /// A remote directory reached over SSH/SFTP: a home server, a NAS, or a
    /// VPS, with nothing installed server-side beyond `sshd`. Authorized by a
    /// directly-entered password or private key rather than an OAuth consent
    /// flow.
    Sftp,
}

impl BackendKind {
    /// Every kind this build knows, in the order the destination picker shows
    /// them. The first entry is the default selection.
    pub const ALL: &'static [BackendKind] = &[
        BackendKind::GoogleDrive,
        BackendKind::S3,
        BackendKind::LocalFolder,
        BackendKind::Sftp,
    ];

    /// The stable stored/wire identifier. This string is written to
    /// `accounts.backend_kind` and crosses the Tauri IPC boundary, so it is
    /// part of the stored format and must not change.
    pub const fn id(self) -> &'static str {
        match self {
            BackendKind::GoogleDrive => "google_drive",
            BackendKind::S3 => "s3",
            BackendKind::LocalFolder => "local_folder",
            BackendKind::Sftp => "sftp",
        }
    }

    /// Whether this backend authenticates through the SPEC s4 OAuth wizard
    /// (client id/secret + PKCE loopback consent) rather than through
    /// directly-entered credentials.
    ///
    /// The setup wizard branches on this to decide which credential step to
    /// render, and `remove_account` branches on it to decide which keychain
    /// entries to purge.
    pub const fn uses_oauth(self) -> bool {
        match self {
            BackendKind::GoogleDrive => true,
            // S3 credentials are an access key pair the user pastes in; there
            // is no consent flow to run.
            BackendKind::S3 => false,
            // A folder the user can already write to needs no credential, so
            // there is nothing to consent to and nothing in the keychain.
            BackendKind::LocalFolder => false,
            // A password or private key the user pastes in; there is no
            // consent flow to run.
            BackendKind::Sftp => false,
        }
    }

    /// Whether this backend can enumerate a browsable folder tree for the
    /// destination picker (`pick_drive_folder`).
    pub const fn supports_folder_picker(self) -> bool {
        match self {
            BackendKind::GoogleDrive => true,
            // S3 "folders" are key prefixes, which the picker can browse the
            // same way it browses Drive folders.
            BackendKind::S3 => true,
            // The destination folder is chosen with the OS folder dialog at
            // setup time, and everything below it is Driven's; browsing the
            // remote tree afterwards would only offer the user a way to nest
            // one backup inside another.
            BackendKind::LocalFolder => false,
            // SFTP readdir drives the same prefix-browse the S3 picker uses.
            BackendKind::Sftp => true,
        }
    }

    /// Whether this backend's destination picker can offer an inline RENAME
    /// on a folder row (issue #307). Mirrors
    /// [`crate::RemoteStore::rename_folder`]'s implementations one-for-one -
    /// this is the flag the picker UI reads to
    /// decide whether to show the affordance at all, so it must never say
    /// `true` for a backend whose store still falls through to the trait's
    /// unsupported default.
    pub const fn supports_rename(self) -> bool {
        match self {
            // `files.update` with just a `name` patch - id and parents
            // untouched.
            BackendKind::GoogleDrive => true,
            // S3 "folders" are key prefixes with no separate identity to
            // rename; doing so for real would mean copying every object under
            // the prefix to a new key and deleting the old ones, a bulk
            // operation the picker's single-row rename does not attempt.
            BackendKind::S3 => false,
            // The destination folder has no browsable tree at all
            // (`supports_folder_picker` is false), so there is no row to
            // rename.
            BackendKind::LocalFolder => false,
            // An SFTP RENAME of the directory, with its sidecar (if any)
            // moved alongside it.
            BackendKind::Sftp => true,
        }
    }

    /// Whether this backend can honour per-source VERSION HISTORY: keeping the
    /// bytes of a superseded file so a point-in-time restore ("restore this
    /// source's files as they were on an earlier date") really returns the older
    /// content.
    ///
    /// This is a property of the backend's storage model, not a feature toggle.
    /// Driven versions a change by forcing the create path and recording the
    /// superseded object as a retained version (see the versioned-change branch
    /// in `driven-core`'s `Executor`, and `resolve_version_supersede`). That
    /// only preserves anything when the superseded bytes end up under an id of
    /// their own. A backend whose object id is a pure function of the file's
    /// name re-writes the same object, so without help the retained
    /// `file_versions` row would end up pointing at an object that now holds the
    /// CURRENT content - a point-in-time restore then hands back today's bytes
    /// while reporting success. Issue #220.
    ///
    /// A name-keyed backend earns a `true` here by implementing
    /// [`RemoteStore::archive_version`], which copies the superseded bytes to a
    /// distinct, content-addressed object in a Driven-owned version store
    /// BEFORE the overwrite. The flag and that implementation are one fact: a
    /// backend that returns `true` here without archiving is back to the #220
    /// bug.
    ///
    /// So this flag gates the offer: the settings UI does not present the
    /// versioning editor where it is false, `set_source_versioning` refuses to
    /// enable it, and the restore path refuses to serve a retained version from
    /// such a destination rather than silently substituting the live object.
    ///
    /// ANY change to a backend's create-key derivation must revisit this arm.
    ///
    /// [`RemoteStore::archive_version`]: crate::remote_store::RemoteStore::archive_version
    pub const fn supports_version_history(self) -> bool {
        match self {
            // A Drive create always mints a NEW file id even for an identical
            // name, and the superseded object is moved to Drive's trash where it
            // stays retrievable by id. The old bytes genuinely survive, so
            // `archive_version` returns `None` (nothing to do).
            BackendKind::GoogleDrive => true,
            // YES, via `archive_version`. The create key is `join_key(parent,
            // name)` and therefore deterministic (filename encryption does not
            // change that - it is SIV-style with a deterministic nonce), so a
            // versioned change would overwrite the previous object. `S3Store`
            // first server-side-copies it to `<root>.driven-versions/...`, which
            // is what the retained version row points at. This is Driven's OWN
            // version store and is independent of the PROVIDER's bucket
            // versioning, which remains the right answer for recovering
            // DELETIONS (`s3Setup.trashWarning` still points users at it).
            BackendKind::S3 => true,
            // YES, by the same mechanism: `driven-localfs` derives a stored
            // object's path from its name, so `LocalFsStore::archive_version`
            // copies the superseded bytes (and their sidecar) into the
            // `.driven-versions` area under the destination root before the
            // overwrite. A copy inside the destination root is a local
            // file-to-file copy, so a retained version costs disk, not traffic.
            BackendKind::LocalFolder => true,
            // NO. SFTP shares the local folder's PATH-keyed model but has no
            // server-side copy primitive: preserving a superseded version would
            // mean downloading the old object and re-uploading it over the very
            // link that is already the bottleneck, doubling the cost of every
            // versioned change. So this stays gated OFF - honestly, with the
            // "this destination keeps no older versions" copy - rather than
            // shipping a feature that silently makes backups twice as slow.
            BackendKind::Sftp => false,
        }
    }

    /// Decodes the persisted `accounts.backend_kind` column.
    ///
    /// `None` and `""` (every pre-`0013` row) decode to
    /// [`BackendKind::GoogleDrive`]. Any other unrecognised value is an error -
    /// see the module docs for why this is deliberately not a fallback.
    pub fn from_stored(stored: Option<&str>) -> anyhow::Result<Self> {
        match stored.map(str::trim) {
            None | Some("") => Ok(BackendKind::GoogleDrive),
            Some(id) => Self::from_id(id),
        }
    }

    /// Parses a [`BackendKind::id`] string. Unlike [`Self::from_stored`] an
    /// empty string is rejected here - this is the IPC-boundary parser, where a
    /// missing value is a caller bug rather than a legacy row.
    pub fn from_id(id: &str) -> anyhow::Result<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.id() == id)
            .ok_or_else(|| anyhow::anyhow!("unknown backup destination backend {id:?}"))
    }

    /// The value to WRITE into `accounts.backend_kind`.
    ///
    /// The historical default is stored as `NULL` so a row created by this
    /// build is byte-identical to a pre-migration row - the column stays a pure
    /// additive marker and a downgrade to an older Driven keeps working for
    /// Drive accounts.
    pub fn to_stored(self) -> Option<&'static str> {
        match self {
            BackendKind::GoogleDrive => None,
            other => Some(other.id()),
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_rows_decode_to_google_drive() {
        assert_eq!(
            BackendKind::from_stored(None).unwrap(),
            BackendKind::GoogleDrive
        );
        assert_eq!(
            BackendKind::from_stored(Some("")).unwrap(),
            BackendKind::GoogleDrive
        );
        assert_eq!(
            BackendKind::from_stored(Some("  ")).unwrap(),
            BackendKind::GoogleDrive
        );
    }

    #[test]
    fn explicit_ids_round_trip() {
        for kind in BackendKind::ALL.iter().copied() {
            assert_eq!(BackendKind::from_id(kind.id()).unwrap(), kind);
            assert_eq!(
                BackendKind::from_stored(kind.to_stored()).unwrap(),
                kind,
                "to_stored/from_stored must round-trip for {kind}"
            );
            // The explicit id always decodes too, even when `to_stored` elides
            // it to NULL - a row written by a future build that stops eliding
            // must still read back correctly.
            assert_eq!(BackendKind::from_stored(Some(kind.id())).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_kinds_are_a_loud_error_not_a_drive_fallback() {
        let err = BackendKind::from_stored(Some("s3_from_the_future")).unwrap_err();
        assert!(err.to_string().contains("s3_from_the_future"));
        assert!(BackendKind::from_id("nope").is_err());
    }

    #[test]
    fn only_backends_that_give_each_version_its_own_object_support_version_history() {
        // Issue #220: per-source versioning records the superseded bytes as a
        // `file_versions` row, which only preserves them when those bytes end up
        // under an id of their own. Drive gets that for free (a create mints a
        // new file id); S3 and the local folder earn it by archiving the
        // superseded object through `RemoteStore::archive_version` before the
        // overwrite. SFTP has no server-side copy, so it stays gated off.
        //
        // The values are pinned, not just asserted present: flipping one
        // silently makes a promise the backend may not keep - the exact shape of
        // the original bug.
        assert!(BackendKind::GoogleDrive.supports_version_history());
        assert!(BackendKind::S3.supports_version_history());
        assert!(BackendKind::LocalFolder.supports_version_history());
        assert!(!BackendKind::Sftp.supports_version_history());
    }

    #[test]
    fn ids_match_serde() {
        for kind in BackendKind::ALL.iter().copied() {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", kind.id()),
                "the serde encoding and BackendKind::id must be the same string"
            );
            let back: BackendKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn ids_are_unique_and_the_default_is_first() {
        let mut ids: Vec<&str> = BackendKind::ALL.iter().map(|k| k.id()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "BackendKind::id values must be unique");
        assert_eq!(BackendKind::ALL[0], BackendKind::default());
    }
}
