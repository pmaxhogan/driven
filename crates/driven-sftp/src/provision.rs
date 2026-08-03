//! The account-creation probe: prove an SFTP destination really works, and
//! stamp (or adopt) its identity marker, BEFORE anything is persisted.
//!
//! This is the SSH counterpart of `driven_localfs::prepare_destination`, and it
//! exists for the same reason: an account that cannot reach its destination is
//! worse than no account, because it sits in the sources list looking
//! configured while every backup cycle fails.
//!
//! ## The order is load-bearing
//!
//! [`prepare_destination`] runs exactly one sequence, and each step depends on
//! the one before it:
//!
//! 1. [`SftpSession::connect_and_pin`] - the ONLY trust-on-first-use entry
//!    point. It proves the host is reachable, that the credential authenticates,
//!    and it records the server's host-key fingerprint into the config so every
//!    later connection can hard-fail on a change. Nothing is written yet.
//! 2. The destination marker ([`names::MARKER_FILE`]) is written - or an
//!    existing one is ADOPTED - and its id recorded on the config. This has to
//!    come before any other write, because [`SftpStore`](crate::store::SftpStore)
//!    verifies the marker on EVERY mutating operation: a probe file written
//!    first would be a write into a directory Driven has not yet proven is its
//!    own, which is precisely the discipline the marker exists to enforce.
//! 3. Only then the writability probe - a real file written and removed - so a
//!    read-only mount, a restrictive ACL or a full filesystem is discovered now
//!    rather than on the first backup cycle.
//!
//! Adoption matters as much here as it does for a re-plugged USB drive: a user
//! re-adding a server that already holds their backups must keep the objects on
//! it, and a freshly-minted destination id would make every one of them
//! invisible.
//!
//! ## What a failure leaves behind
//!
//! A failure after step 2 can leave an ORPHAN marker on the server: a file
//! naming a destination no account will ever claim. That is inert by design -
//! the marker only ever grants access to an account whose config carries the
//! same id, and the next successful probe against that root adopts it rather
//! than stacking a second one. Cleaning it up would mean deleting a file from a
//! stranger's directory on a failure path, which is a worse trade than leaving
//! a few hundred harmless bytes.

use russh_sftp::client::SftpSession as RusshSftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::AsyncWriteExt;

use crate::config::{DestinationMarker, SftpConfig, SftpCredential};
use crate::names;
use crate::session::SftpSession;
use crate::store::{dest_missing, is_no_such_file, join_remote, sftp_io_error, sftp_op_error};

/// The bytes the writability probe writes. Content is irrelevant beyond being
/// non-empty and obviously Driven's, in case a probe file ever survives a crash
/// long enough for someone to find it.
const PROBE_CONTENT: &[u8] = b"driven writability probe";

/// What [`prepare_destination`] found at the account's `root_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedDestination {
    /// The root held no marker, so this probe stamped a fresh one.
    Initialized,
    /// The root already carried a Driven marker, whose destination id was
    /// adopted - every object already on the server stays reachable.
    Adopted,
}

/// Probe an SFTP destination for account creation, filling in the two fields a
/// new [`SftpConfig`] cannot know until the server has been contacted: the
/// pinned `host_key_fingerprint` and the `destination_id`.
///
/// `config` is NORMALIZED in place before anything else, so the caller stores
/// exactly what was probed. On success both fields are `Some`; on failure
/// nothing about the account has been persisted anywhere (this function touches
/// neither the keychain nor SQLite - see the module docs for the one artefact a
/// late failure can leave on the SERVER).
///
/// The errors are the classified ones the rest of the crate produces, so the
/// wizard can tell the four cases apart that the user has to act on
/// differently: the host is unreachable (network), the credential was refused
/// (`auth.invalid_grant`), the root path does not exist (`sftp.root_missing`),
/// and the root exists but belongs to a DIFFERENT Driven destination
/// (`sftp.dest_marker_mismatch`).
pub async fn prepare_destination(
    config: &mut SftpConfig,
    credential: &SftpCredential,
    now_ms: i64,
) -> anyhow::Result<PreparedDestination> {
    *config = config.clone().normalized()?;

    // 1) Reachability + auth + trust-on-first-use pinning.
    let session = SftpSession::connect_and_pin(config, credential).await?;
    let sftp = session.sftp();
    let root = config.root_path.clone();

    // The root is never CREATED: a typo must surface as an error rather than
    // quietly starting a backup in a brand-new directory beside the intended
    // one. `sftp.root_missing` is the code the wizard keys on to say so.
    match sftp.metadata(root.clone()).await {
        Ok(attrs) if attrs.is_dir() => {}
        Ok(_) => {
            return Err(dest_missing(
                "sftp.root_not_a_directory",
                &format!("{root} exists on {} but is not a directory", config.host),
            ))
        }
        Err(error) if is_no_such_file(&error) => {
            return Err(dest_missing(
                "sftp.root_missing",
                &format!(
                    "the configured root path {root} does not exist on {}",
                    config.host
                ),
            ))
        }
        Err(error) => return Err(sftp_op_error(&format!("stat {root}"), error)),
    }

    // 2) The identity marker, before any other write.
    let outcome = stamp_or_adopt_marker(sftp, config, &root, now_ms).await?;

    // 3) A real write, proving the credential's user can actually put bytes
    // here. A permission bit says nothing: a read-only export, a restrictive
    // ACL and a full filesystem all pass inspection and then fail the first
    // upload, leaving an account that looks configured and never backs
    // anything up.
    probe_writable(sftp, &root).await?;

    tracing::info!(
        target: crate::TARGET,
        host = %config.host,
        port = config.port,
        root_path = %root,
        ?outcome,
        "prepared an SFTP destination"
    );
    Ok(outcome)
}

/// Adopt the marker already at `root`, or write a fresh one, recording the id
/// on `config` either way.
async fn stamp_or_adopt_marker(
    sftp: &RusshSftpSession,
    config: &mut SftpConfig,
    root: &str,
    now_ms: i64,
) -> anyhow::Result<PreparedDestination> {
    let path = join_remote(root, names::MARKER_FILE);
    match sftp.read(path.clone()).await {
        Ok(raw) => match serde_json::from_slice::<DestinationMarker>(&raw) {
            Ok(marker) if !marker.destination_id.trim().is_empty() => {
                let found = marker.destination_id.trim().to_string();
                // A config that ALREADY carries an id is being re-probed (a
                // reconnect), and adopting a different id here would silently
                // repoint the account at someone else's backup tree - the exact
                // move `SftpStore::guard_root` refuses on every mutating op.
                if let Some(expected) = config.destination_id.as_deref() {
                    if expected != found {
                        return Err(dest_missing(
                            "sftp.dest_marker_mismatch",
                            &format!(
                                "{root} holds a different Driven destination \
                                 (expected {expected}, found {found})"
                            ),
                        ));
                    }
                }
                config.destination_id = Some(found);
                return Ok(PreparedDestination::Adopted);
            }
            // An unreadable or id-less marker proves nothing, so it is replaced
            // rather than trusted - matching `driven_localfs::prepare_destination`.
            // (`SftpStore::guard_root` still FAILS CLOSED on one; only this
            // deliberate, user-initiated setup step may overwrite it.)
            _ => {
                tracing::warn!(
                    target: crate::TARGET,
                    %path,
                    "the destination marker is unreadable; re-initializing it"
                );
            }
        },
        Err(error) if is_no_such_file(&error) => {}
        // Any other protocol failure is a bad connection, not a missing
        // destination: reporting it as "not a Driven destination" would tell
        // the user to fix the wrong thing.
        Err(error) => return Err(sftp_op_error(&format!("read {path}"), error)),
    }

    let destination_id = uuid::Uuid::new_v4().to_string();
    let marker = DestinationMarker::new(&destination_id, now_ms);
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|e| anyhow::anyhow!("sftp.config_invalid: could not encode the marker: {e}"))?;
    write_file(sftp, &path, &bytes).await?;
    config.destination_id = Some(destination_id);
    Ok(PreparedDestination::Initialized)
}

/// Write and then remove a real file under `root`.
async fn probe_writable(sftp: &RusshSftpSession, root: &str) -> anyhow::Result<()> {
    let name = format!("{}{}", names::TMP_PREFIX, uuid::Uuid::new_v4());
    let path = join_remote(root, &name);
    write_file(sftp, &path, PROBE_CONTENT).await?;
    match sftp.remove_file(path.clone()).await {
        Ok(()) => Ok(()),
        Err(error) if is_no_such_file(&error) => Ok(()),
        // A destination that accepts writes but refuses deletes cannot be
        // backed up to: every upload commits through a temp file that is
        // renamed away, and every superseded object is removed. Better to
        // refuse the account than to discover it one cycle later.
        Err(error) => Err(sftp_op_error(&format!("remove {path}"), error)),
    }
}

/// Create (or truncate) `path` and write `bytes`.
async fn write_file(sftp: &RusshSftpSession, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    // CREATE has to be explicit - `SftpSession::write` opens WRITE-only, which
    // a correct server answers with SSH_FX_NO_SUCH_FILE for a new file.
    let mut file = sftp
        .open_with_flags(
            path.to_string(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| sftp_op_error(&format!("create {path}"), e))?;
    file.write_all(bytes)
        .await
        .map_err(|e| sftp_io_error(&format!("write {path}"), e))?;
    file.shutdown()
        .await
        .map_err(|e| sftp_io_error(&format!("close {path}"), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SftpAuthKind;
    use crate::test_support::TestSftpServer;
    use driven_remote::remote_store::DriveErrorClassification;
    use driven_remote::DriveError;

    const NOW: i64 = 1_700_000_000_000;

    /// The marker sitting in `root`, as the server's own filesystem sees it.
    fn marker_in(root: &std::path::Path) -> DestinationMarker {
        let raw = std::fs::read(root.join(names::MARKER_FILE)).expect("a marker was written");
        serde_json::from_slice(&raw).expect("the marker parses")
    }

    fn names_in(root: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .expect("read the served directory")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn classification(error: &anyhow::Error) -> Option<DriveErrorClassification> {
        error
            .downcast_ref::<DriveError>()
            .map(DriveError::classification)
    }

    #[tokio::test]
    async fn a_successful_probe_pins_the_host_key_and_stamps_a_marker() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.unpinned_config(SftpAuthKind::Password);

        let outcome = prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect("a reachable server with a good password prepares");

        assert_eq!(outcome, PreparedDestination::Initialized);
        assert_eq!(
            config.host_key_fingerprint.as_deref(),
            Some(server.host_key_fingerprint()),
            "the probe is the only trust-on-first-use point, so it must record what it saw"
        );
        let marker = marker_in(server.root());
        assert_eq!(
            config.destination_id.as_deref(),
            Some(marker.destination_id.as_str()),
            "the config's id and the marker's must agree or every later write is refused"
        );
        // The writability probe cleans up after itself: the marker is the only
        // thing a fresh destination is left holding.
        assert_eq!(
            names_in(server.root()),
            vec![names::MARKER_FILE.to_string()]
        );
    }

    #[tokio::test]
    async fn a_private_key_credential_probes_the_same_way() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.unpinned_config(SftpAuthKind::PrivateKey);
        prepare_destination(&mut config, &server.key_credential(), NOW)
            .await
            .expect("key auth prepares");
        assert!(config.host_key_fingerprint.is_some());
        assert!(config.destination_id.is_some());
    }

    #[tokio::test]
    async fn a_second_probe_adopts_the_existing_destination_id() {
        // Re-adding a server that already holds a backup must keep every object
        // on it; a freshly-minted id would make all of them invisible.
        let server = TestSftpServer::spawn().await.unwrap();
        let mut first = server.unpinned_config(SftpAuthKind::Password);
        prepare_destination(&mut first, &server.password_credential(), NOW)
            .await
            .expect("first probe");

        let mut second = server.unpinned_config(SftpAuthKind::Password);
        let outcome = prepare_destination(&mut second, &server.password_credential(), NOW + 1)
            .await
            .expect("second probe");

        assert_eq!(outcome, PreparedDestination::Adopted);
        assert_eq!(second.destination_id, first.destination_id);
        assert_eq!(
            marker_in(server.root()).created_at_ms,
            NOW,
            "adoption must not re-stamp the marker"
        );
    }

    #[tokio::test]
    async fn a_re_probe_refuses_a_root_holding_a_different_destination() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.unpinned_config(SftpAuthKind::Password);
        prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect("first probe");
        config.destination_id = Some("some-other-destination".to_string());

        let error = prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect_err("adopting a stranger's marker would repoint the account");
        assert!(
            format!("{error:?}").contains("sftp.dest_marker_mismatch"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_unreadable_marker_is_replaced_rather_than_trusted() {
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::write(server.root().join(names::MARKER_FILE), b"{ truncated").unwrap();

        let mut config = server.unpinned_config(SftpAuthKind::Password);
        let outcome = prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect("a corrupt marker does not block SETUP (only mutating ops fail closed)");

        assert_eq!(outcome, PreparedDestination::Initialized);
        assert_eq!(
            config.destination_id.as_deref(),
            Some(marker_in(server.root()).destination_id.as_str())
        );
    }

    #[tokio::test]
    async fn a_missing_root_path_is_reported_as_such_and_never_created() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = SftpConfig {
            root_path: "/not-there".to_string(),
            ..server.unpinned_config(SftpAuthKind::Password)
        };

        let error = prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect_err("a typo in the root path must not silently create a directory");
        assert!(
            format!("{error:?}").contains("sftp.root_missing"),
            "the wizard keys on this exact code: {error:?}"
        );
        assert!(!server.root().join("not-there").exists());
        assert!(names_in(server.root()).is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn a_root_path_that_is_a_file_is_refused() {
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::write(server.root().join("a-file"), b"not a directory").unwrap();
        let mut config = SftpConfig {
            root_path: "/a-file".to_string(),
            ..server.unpinned_config(SftpAuthKind::Password)
        };

        let error = prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect_err("a file is not a destination");
        assert!(
            format!("{error:?}").contains("sftp.root_not_a_directory"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_credential_fails_before_anything_is_written() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.unpinned_config(SftpAuthKind::Password);

        let error = prepare_destination(
            &mut config,
            &SftpCredential::Password {
                password: "wrong-password".to_string(),
            },
            NOW,
        )
        .await
        .expect_err("a bad password cannot prepare a destination");

        assert_eq!(
            classification(&error),
            Some(DriveErrorClassification::AuthInvalidGrant),
            "{error:?}"
        );
        assert!(config.host_key_fingerprint.is_none(), "nothing was pinned");
        assert!(config.destination_id.is_none());
        assert!(names_in(server.root()).is_empty(), "nothing was written");
    }

    #[tokio::test]
    async fn an_unreachable_host_classifies_as_network() {
        // A port nothing is listening on: bind one, learn its number, drop it.
        let dead_port = {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut config = SftpConfig {
            host: "127.0.0.1".to_string(),
            port: dead_port,
            root_path: "/".to_string(),
            username: "nobody".to_string(),
            auth: SftpAuthKind::Password,
            host_key_fingerprint: None,
            destination_id: None,
        };

        let error = prepare_destination(
            &mut config,
            &SftpCredential::Password {
                password: "irrelevant".to_string(),
            },
            NOW,
        )
        .await
        .expect_err("nothing is listening there");
        assert_eq!(
            classification(&error),
            Some(DriveErrorClassification::Network),
            "{error:?}"
        );
        assert!(config.host_key_fingerprint.is_none());
    }

    #[tokio::test]
    async fn the_probe_prepares_a_nested_root_path() {
        // The common shape: a user points Driven at /srv/backups/driven rather
        // than the server's login directory.
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::create_dir_all(server.root().join("srv/backups")).unwrap();
        let mut config = SftpConfig {
            root_path: "/srv/backups/".to_string(),
            ..server.unpinned_config(SftpAuthKind::Password)
        };

        prepare_destination(&mut config, &server.password_credential(), NOW)
            .await
            .expect("a nested root prepares");
        assert_eq!(config.root_path, "/srv/backups", "normalized in place");
        assert_eq!(
            names_in(&server.root().join("srv/backups")),
            vec![names::MARKER_FILE.to_string()]
        );
    }
}
