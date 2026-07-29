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
}

impl BackendKind {
    /// Every kind this build knows, in the order the destination picker shows
    /// them. The first entry is the default selection.
    pub const ALL: &'static [BackendKind] = &[BackendKind::GoogleDrive, BackendKind::S3];

    /// The stable stored/wire identifier. This string is written to
    /// `accounts.backend_kind` and crosses the Tauri IPC boundary, so it is
    /// part of the stored format and must not change.
    pub const fn id(self) -> &'static str {
        match self {
            BackendKind::GoogleDrive => "google_drive",
            BackendKind::S3 => "s3",
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
