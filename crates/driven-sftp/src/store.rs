//! [`SftpStore`] - the `RemoteStore` implementation over SSH: a home server, a
//! NAS, or a VPS you can already reach with `ssh`, with nothing installed on
//! the far end.
//!
//! It is modelled on `driven-localfs` rather than `driven-s3`, because SFTP is
//! a filesystem over the wire: directories really have to exist, a name has to
//! be legal for the REMOTE filesystem, and a rename is the only atomic publish
//! primitive available. What it borrows from `driven-s3` is the network
//! discipline - every failure is classified rather than bubbled raw, and a lost
//! connection is a retryable outcome instead of a fatal one.
//!
//! ## Integrity (read this before touching the upload paths)
//!
//! The executor verifies every upload by comparing the `RemoteEntry.md5` this
//! store returns against the md5 it computed locally over the exact bytes it
//! sent (`executor.rs`, "md5 verify over the exact bytes sent"). SFTP has no
//! server-computed digest to hand back - there is no `ETag` and no
//! `md5Checksum` field - so the naive implementation, returning the digest we
//! accumulated while writing, would make the executor compare a number against
//! itself and silently disable corruption detection for every file.
//!
//! So [`SftpStore`] never returns its in-memory digest. Every write:
//!
//! 1. streams into a temp file in the target's own remote directory, hashing as
//!    it goes;
//! 2. removes the target if it is there, because SFTPv3 has no overwriting
//!    rename (see below);
//! 3. `SSH_FXP_RENAME`s the temp file into place;
//! 4. **re-downloads the committed object and hashes it back off the remote**,
//!    and returns THAT digest.
//!
//! Step 4 is what makes the executor's check real: the bytes make a full round
//! trip through the server's own filesystem and back across the wire. Its
//! honest limitation is the same one the local backend documents - the re-read
//! can be served from the server's page cache, so it proves the object was
//! correctly assembled and correctly named rather than that the platters hold
//! it.
//!
//! **Removing step 4 turns every upload check into `x == x`.** It is
//! load-bearing, not belt-and-braces.
//!
//! ## Why the target is removed before the rename
//!
//! SFTPv3 `SSH_FXP_RENAME` has no overwrite flag, and OpenSSH's v3 handler
//! implements it with `link()` + `unlink()` precisely so the rename is
//! race-free - which means it fails with `SSH_FX_FAILURE` when the destination
//! exists. (The `posix-rename@openssh.com` extension does overwrite atomically,
//! but it is an extension, `russh-sftp` does not expose it, and a backend that
//! only worked against OpenSSH would be worse than one that works everywhere.)
//!
//! So an UPDATE is remove-then-rename, and that window is real: a crash between
//! the two leaves the object absent rather than half-written. That is the
//! failure mode to prefer of the two available - a missing object is re-created
//! by the replaying pending op at exactly the same path, whereas a
//! partially-overwritten one would be indistinguishable from a good one - but
//! it IS weaker than the local backend's atomic rename, and it is stated here
//! rather than papered over.
//!
//! ## Proving the destination is the destination
//!
//! `root_path` is a string the user typed, and a directory holding the user's
//! OWN data is indistinguishable from an initialized-but-empty destination by
//! inspection alone. That is not a cosmetic problem here, because two of this
//! store's behaviours are destructive by design when aimed at foreign data:
//! [`SftpStore::resolve_stored_name`] ADOPTS an unannotated name it finds at a
//! candidate path (correct for a crashed upload's leftovers), and
//! [`SftpStore::commit_object`] REMOVES the target before renaming over it.
//! Pointed at `/home/user/Documents`, those two would quietly destroy
//! same-named files.
//!
//! So every MUTATING operation first proves the root carries Driven's
//! destination marker ([`crate::names::MARKER_FILE`], the same filename and
//! schema the local-folder backend uses), and that its `destinationId` is this
//! account's - see [`SftpStore::guard_root`]. It also catches the server-side
//! analogue of an unplugged stick: a NAS volume or array that is not mounted
//! this cycle leaves an ordinary empty directory at the mount point, and a
//! whole backup written into it vanishes on the next remount while
//! `file_state` still calls every file synced.
//!
//! READS are deliberately not gated; the reasoning and the account-creation
//! ordering it depends on are in [`SftpStore::guard_root`]'s docs and in the
//! `reads_are_deliberately_not_marker_gated` test.
//!
//! ## Trash
//!
//! **SSH has no trash**, and Driven does not simulate one by moving objects
//! into a hidden remote folder: nothing would ever empty it, so a backup
//! destination would grow without bound and fill the very disk it lives on.
//! `trash` is therefore a permanent delete, identical to `delete_permanent`,
//! exactly as the S3 and local-folder backends do - and the setup UI says so
//! rather than pretending otherwise.
//!
//! ## Connection lifecycle
//!
//! One [`SftpSession`] per store, established LAZILY on the first operation
//! (constructing a store must not require the server to be up - an account
//! whose NAS is asleep has to survive app start) and re-established
//! transparently when the transport dies. The session lives behind an
//! `RwLock`: ordinary operations share a read guard and run concurrently, since
//! `russh-sftp` multiplexes requests over one channel; only a reconnect takes
//! the write guard.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use driven_remote::remote_store::{
    AboutInfo, DownloadStream, DriveContext, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, UploadBody,
};
use driven_remote::DriveError;
use futures::StreamExt;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use russh_sftp::client::error::Error as SftpProtocolError;
use russh_sftp::client::SftpSession as RusshSftpSession;
use russh_sftp::protocol::{OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{SftpConfig, SftpCredential};
use crate::error::{sftp_error, status_code, SftpFailure};
use crate::meta::{self, EntryKind, Sidecar};
use crate::names;
use crate::session::SftpSession;

/// MIME type reported for a directory entry.
const FOLDER_MIME: &str = "application/x-directory";

/// Default MIME type for an object whose sidecar records none.
const DEFAULT_MIME: &str = "application/octet-stream";

/// Read buffer for the post-commit verify pass and for downloads.
const READ_CHUNK: usize = 256 * 1024;

/// How many sidecar reads a directory-wide scan keeps in flight.
///
/// A listing needs one sidecar per entry and `russh-sftp` multiplexes requests
/// over the single channel, so issuing them concurrently turns N round trips
/// into roughly one. Bounded rather than unbounded because a folder with tens
/// of thousands of objects would otherwise queue that many outstanding requests
/// against a server that has its own limits.
const SIDECAR_FETCH_CONCURRENCY: usize = 16;

// -- object ids ---------------------------------------------------------------
//
// An id is a `/`-separated path RELATIVE to the account's `root_path`, built
// from `names::encode`d components. `""` is the root; a FOLDER id ends in `/`,
// a FILE id does not. This is the `driven-localfs` convention, byte for byte,
// which is what lets a tree be moved between an SFTP destination and a local
// one without re-uploading anything.

/// Normalize a caller-supplied folder id to the canonical slash-terminated form
/// (`""` stays `""`).
fn folder_prefix(folder_id: &str) -> String {
    if folder_id.is_empty() || folder_id.ends_with('/') {
        folder_id.to_string()
    } else {
        format!("{folder_id}/")
    }
}

/// The id of `stored` (already encoded) under the folder `parent_id`.
fn join_id(parent_id: &str, stored: &str) -> String {
    format!("{}{}", folder_prefix(parent_id), stored)
}

/// The folder id for `stored` under `parent_id`.
fn folder_id(parent_id: &str, stored: &str) -> String {
    format!("{}/", join_id(parent_id, stored.trim_end_matches('/')))
}

/// The last component of an id (`""` for the root).
fn base_name(id: &str) -> &str {
    id.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// The parent folder id of an id (`""` at the root).
fn parent_of(id: &str) -> String {
    let trimmed = id.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => trimmed[..=i].to_string(),
        None => String::new(),
    }
}

/// Join a remote directory path and a single component, SFTP-style (always
/// `/`, never the host platform's separator).
fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// -- errors -------------------------------------------------------------------

/// The error for "this object is not on the remote".
///
/// Kept distinct from a generic protocol failure because `metadata` on a
/// missing object is an ANSWER, not a fault, and callers match on it.
fn not_found(id: &str) -> anyhow::Error {
    anyhow::Error::new(DriveError::Classified {
        kind: driven_remote::remote_store::DriveErrorClassification::Other,
        source: anyhow::anyhow!("sftp.not_found: no object {id:?} on the server"),
    })
}

/// The error for "the configured root is not (or is no longer) this account's
/// Driven destination".
///
/// Shaped exactly like `driven_localfs::error::dest_missing`: the returned
/// error IS a [`DriveError::DestFolderMissing`], so it carries the SPEC s24
/// `drive.dest_folder_missing` code the tray already surfaces and the "the user
/// must act" semantics the engine already implements. The `sftp.`-prefixed
/// reason rides on top as anyhow context, so a log line says WHICH of the
/// several causes fired without inventing a second classification for a
/// condition the app already knows how to handle.
fn dest_missing(code: &str, reason: &str) -> anyhow::Error {
    tracing::warn!(
        target: crate::TARGET,
        %code,
        %reason,
        "refusing to write: the SFTP destination is unavailable or is not a Driven destination"
    );
    anyhow::Error::new(DriveError::DestFolderMissing).context(format!("{code}: {reason}"))
}

/// Translate a `russh-sftp` protocol error into Driven's classified taxonomy.
///
/// `SSH_FXP_STATUS` replies keep their numeric code so `crate::error`'s table
/// can do its job (ENOSPC-shaped -> `StorageQuota`, generic failure ->
/// retryable, everything else fatal for this op). A transport-level error is
/// connection loss, which is retryable; a protocol-level surprise
/// (`UnexpectedPacket`, a limit violation) is reported as a bad message, which
/// is not - retrying a malformed exchange just repeats it.
fn sftp_op_error(context: &str, error: SftpProtocolError) -> anyhow::Error {
    let failure = match &error {
        SftpProtocolError::Status(status) => SftpFailure::Status {
            code: status.status_code as u32,
            message: format!("{context}: {}", status.error_message),
        },
        SftpProtocolError::Timeout => SftpFailure::ConnectionLost {
            detail: format!("{context}: the server did not answer in time"),
        },
        SftpProtocolError::IO(detail) => SftpFailure::ConnectionLost {
            detail: format!("{context}: {detail}"),
        },
        other => SftpFailure::Status {
            code: status_code::BAD_MESSAGE,
            message: format!("{context}: {other}"),
        },
    };
    anyhow::Error::new(sftp_error(failure))
}

/// Translate an [`std::io::Error`] raised by a `russh-sftp` `File`'s
/// `AsyncRead`/`AsyncWrite` into the same taxonomy.
///
/// Verified against `russh-sftp` 2.3.0 (`client/fs/file.rs`): a file handle
/// collapses an `SSH_FXP_STATUS` into `io::Error::other(status.error_message)`,
/// **discarding the numeric code**. The MESSAGE survives, which is what
/// `crate::error::is_enospc_shaped` reads, so a full remote disk still
/// classifies as `StorageQuota`; anything else is reported as the generic
/// server-side failure it is (retryable), except a broken pipe, which is the
/// transport going away.
fn sftp_io_error(context: &str, error: std::io::Error) -> anyhow::Error {
    let code = if error.kind() == std::io::ErrorKind::BrokenPipe {
        status_code::CONNECTION_LOST
    } else {
        status_code::FAILURE
    };
    anyhow::Error::new(sftp_error(SftpFailure::Status {
        code,
        message: format!("{context}: {error}"),
    }))
}

/// Is `error` an `SSH_FX_NO_SUCH_FILE`?
fn is_no_such_file(error: &SftpProtocolError) -> bool {
    matches!(error, SftpProtocolError::Status(status) if status.status_code == StatusCode::NoSuchFile)
}

/// Does this connect failure look like a PERMANENT protocol incompatibility
/// rather than a transient network fault?
///
/// russh reports "no common <kind> algorithm - ours: [...], theirs: [...]"
/// (verified in russh 0.62.5 `negotiation.rs`) and a handful of neighbouring
/// key-exchange failures. The session layer classifies all of them as
/// `Connect` -> `Network`, which the executor retries with backoff forever - so
/// a server Driven simply cannot speak to looks exactly like one that is
/// briefly down.
fn looks_like_algorithm_incompatibility(chain: &str) -> bool {
    let lower = chain.to_lowercase();
    (lower.contains("no common") && lower.contains("algorithm"))
        || lower.contains("unknown algorithm")
        || lower.contains("key exchange failed")
        || lower.contains("key exchange init failed")
        || lower.contains("invalid ssh version string")
}

// -- destination name claims --------------------------------------------------

/// The set of remote names currently being written, so a probe can see a claim
/// that has not been committed to a sidecar yet.
///
/// Keyed by `(folder id, ASCII-lowercased stored name)`. The lowercasing is a
/// cheap approximation of the remote filesystem's folding used ONLY to decide
/// which claims to compare; the authoritative answer still comes from the
/// sidecar probe, so a folding this misses degrades to the same window an
/// external writer has, not to a wrong answer.
#[derive(Default)]
struct NameClaims {
    inner: Mutex<HashMap<(String, String), (String, usize)>>,
}

/// A live claim on a remote name. Released on drop.
struct ClaimGuard {
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

/// Try to claim `candidate` for `original`.
///
/// `None` means a DIFFERENT original name already holds it. `Some((guard,
/// fresh))` hands over the claim; `fresh` is true when this is the first claim
/// on the name, i.e. when the caller still has to probe the remote. A
/// non-fresh claim was already probed by the sibling upload that took it, so
/// re-probing would only spend a round trip to learn the same thing.
///
/// Deliberately synchronous and lock-free of any await: the lock is a
/// `parking_lot::Mutex` held for a single map edit. The remote probe happens
/// AFTER the tentative claim, and a probe that comes back "taken" simply drops
/// the guard and moves to the next candidate.
fn try_claim(
    claims: &Arc<NameClaims>,
    dir_id: &str,
    candidate: &str,
    original: &str,
) -> Option<(ClaimGuard, bool)> {
    let key = (dir_id.to_string(), candidate.to_ascii_lowercase());
    let mut map = claims.inner.lock();
    let fresh = match map.get(&key) {
        Some((holder, _)) if holder != original => return None,
        Some(_) => false,
        None => true,
    };
    let entry = map
        .entry(key.clone())
        .or_insert_with(|| (original.to_string(), 0));
    entry.1 += 1;
    Some((
        ClaimGuard {
            claims: Arc::clone(claims),
            key,
        },
        fresh,
    ))
}

/// Claim a remote name that has ALREADY been decided, without probing.
///
/// An `update` owns its target by definition (the caller carried the id), so
/// re-probing would be wrong: the sidecar for the NEW content is not committed
/// yet, and a probe could hand the name to someone else mid-write.
fn claim_exact(claims: &Arc<NameClaims>, dir_id: &str, stored: &str, original: &str) -> ClaimGuard {
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

// -- the store ----------------------------------------------------------------

/// The SSH (SFTP) [`RemoteStore`].
pub struct SftpStore {
    config: SftpConfig,
    credential: SftpCredential,
    /// The account's `root_path`, normalized: absolute, and without a trailing
    /// slash unless it IS the bare root.
    root: String,
    session: tokio::sync::RwLock<Option<SftpSession>>,
    claims: Arc<NameClaims>,
}

/// A borrowed, live SFTP channel.
///
/// Holding this keeps the store's session from being swapped out underneath the
/// operation; it is a read guard, so any number of operations hold one at once.
struct Channel<'a> {
    guard: tokio::sync::RwLockReadGuard<'a, Option<SftpSession>>,
}

impl Channel<'_> {
    fn sftp(&self) -> &RusshSftpSession {
        self.guard
            .as_ref()
            .expect("a Channel is only ever built around a live session")
            .sftp()
    }
}

impl SftpStore {
    /// Build a store for `config`, authenticating with `credential`.
    ///
    /// Does NOT connect: a home server or NAS is routinely asleep, and failing
    /// here would leave the account unusable until the user happened to have
    /// the box awake at app start. The connection is established on the first
    /// operation and re-established transparently after a drop.
    pub fn new(config: &SftpConfig, credential: &SftpCredential) -> anyhow::Result<Self> {
        let config = config.clone().normalized()?;
        let root = config.root_path.clone();
        Ok(Self {
            config,
            credential: credential.clone(),
            root,
            session: tokio::sync::RwLock::new(None),
            claims: Arc::new(NameClaims::default()),
        })
    }

    /// The destination root "folder" id: the empty relative path.
    pub fn root_id(&self) -> &str {
        ""
    }

    /// The configuration this store was built from.
    pub fn config(&self) -> &SftpConfig {
        &self.config
    }

    // -- connection ----------------------------------------------------------

    /// A live SFTP channel, connecting or reconnecting if needed.
    async fn channel(&self) -> anyhow::Result<Channel<'_>> {
        {
            let guard = self.session.read().await;
            if guard.as_ref().is_some_and(|s| s.is_connected()) {
                return Ok(Channel { guard });
            }
        }
        {
            let mut guard = self.session.write().await;
            match guard.as_mut() {
                Some(session) if session.is_connected() => {}
                Some(session) => session
                    .reconnect()
                    .await
                    .map_err(Self::note_connect_failure)?,
                None => {
                    let session = SftpSession::connect(&self.config, &self.credential)
                        .await
                        .map_err(Self::note_connect_failure)?;
                    *guard = Some(session);
                }
            }
        }
        let guard = self.session.read().await;
        if guard.as_ref().is_some_and(|s| s.is_connected()) {
            return Ok(Channel { guard });
        }
        // The window between dropping the write guard and taking the read one
        // is tiny but real: the transport can die inside it. Report it as what
        // it is rather than unwrapping a session that is already gone.
        Err(anyhow::Error::new(sftp_error(
            SftpFailure::ConnectionLost {
                detail: format!(
                    "the connection to {}:{} was lost immediately after it was established",
                    self.config.host, self.config.port
                ),
            },
        )))
    }

    /// Log a connect failure that is really a permanent incompatibility.
    ///
    /// Task 2 flagged this and it is deliberately NOT a reclassification: the
    /// session layer maps every handshake failure to `Connect` -> `Network`,
    /// the executor retries `Network` with backoff, and changing the class here
    /// would change retry behaviour the engine depends on for genuinely
    /// transient faults. The UI surfacing lands with the wizard; until then a
    /// warn line is what tells an operator reading the log that the retries
    /// will never succeed.
    fn note_connect_failure(error: DriveError) -> DriveError {
        let chain = format!("{error:?}");
        if looks_like_algorithm_incompatibility(&chain) {
            tracing::warn!(
                target: crate::TARGET,
                %chain,
                "the SSH handshake failed during algorithm negotiation - this is a PERMANENT \
                 incompatibility, not a transient network fault, and it is currently classified \
                 as a network error so it will be retried with backoff indefinitely. A server \
                 whose only host key is RSA reaches this: Driven's SSH stack is built without \
                 the RSA backend (see driven_sftp::session::unsupported_key_algorithm)"
            );
        }
        error
    }

    // -- destination identity ------------------------------------------------

    /// The destination marker's absolute remote path.
    fn marker_path(&self) -> String {
        join_remote(&self.root, names::MARKER_FILE)
    }

    /// Prove the configured root really is this account's Driven destination,
    /// before writing anything into it.
    ///
    /// `root_path` is a string the user typed, and a directory holding the
    /// user's OWN data is indistinguishable from an initialized-but-empty
    /// destination by inspection alone. That matters here more than anywhere
    /// else in this crate, because two of the store's own behaviours are
    /// destructive by design when pointed at foreign data:
    /// [`Self::resolve_stored_name`] ADOPTS an unannotated name it finds
    /// sitting at a candidate path (correct for a crashed upload's leftovers),
    /// and [`Self::commit_object`] REMOVES the target before renaming over it.
    /// Aimed at `/home/user/Documents`, those two would quietly destroy
    /// same-named files. The marker is what makes that impossible.
    ///
    /// It also catches the server-side analogue of the unplugged stick: a NAS's
    /// external volume or an array that is not mounted this cycle leaves an
    /// ordinary empty directory at the mount point, and writing a whole backup
    /// into it would put the data somewhere the next remount hides forever
    /// while `file_state` still called every file synced.
    ///
    /// Deliberately NOT cached, matching `driven_localfs::LocalFsStore::guard_root`:
    /// re-reading per operation is the entire point, because a volume can go
    /// away mid-backup and a cached "yes" would keep writing into the hole. The
    /// cost is one round trip per mutating operation.
    ///
    /// ## What is a missing destination and what is a bad connection
    ///
    /// Only `SSH_FX_NO_SUCH_FILE` and an unparseable marker are treated as "not
    /// a destination". Any OTHER protocol failure propagates as itself, so a
    /// flapping link stays a retryable network error instead of telling the
    /// user their backup destination vanished. This is a deliberate divergence
    /// from the local backend, which maps every marker read error to
    /// `dest_missing` - correct there, where a failed local read really does
    /// mean the medium is gone, and wrong over a network.
    async fn guard_root(&self, sftp: &RusshSftpSession) -> anyhow::Result<()> {
        let path = self.marker_path();
        let raw = match sftp.read(path.clone()).await {
            Ok(raw) => raw,
            Err(error) if is_no_such_file(&error) => {
                // One extra round trip, on the failure path only, to tell the
                // two very different user actions apart: "your root_path is
                // wrong" versus "this directory is not a Driven destination".
                return Err(match Self::stat_kind(sftp, &self.root).await? {
                    None => dest_missing(
                        "sftp.root_missing",
                        &format!(
                            "the configured root path {} does not exist on {}",
                            self.root, self.config.host
                        ),
                    ),
                    Some(_) => dest_missing(
                        "sftp.dest_marker_missing",
                        &format!(
                            "{path} is not there, so {} is not a Driven destination (or its \
                             volume is not mounted). Driven will not write into a directory it \
                             cannot prove is its own",
                            self.root
                        ),
                    ),
                });
            }
            Err(error) => return Err(sftp_op_error(&format!("read {path}"), error)),
        };

        let marker: crate::config::DestinationMarker = match serde_json::from_slice(&raw) {
            Ok(marker) => marker,
            Err(error) => {
                return Err(dest_missing(
                    "sftp.dest_marker_unreadable",
                    &format!("the destination marker at {path} is unreadable: {error}"),
                ))
            }
        };

        match self.config.destination_id.as_deref() {
            Some(expected) if expected != marker.destination_id.trim() => Err(dest_missing(
                "sftp.dest_marker_mismatch",
                &format!(
                    "{} holds a different Driven destination (expected {expected}, found {})",
                    self.root,
                    marker.destination_id.trim()
                ),
            )),
            Some(_) => Ok(()),
            // An account written before `destination_id` existed. The marker's
            // presence still proves this is a Driven destination, which is the
            // half that stops the destructive cases; only the "a DIFFERENT
            // Driven destination is here" check is unavailable.
            None => {
                tracing::warn!(
                    target: crate::TARGET,
                    root = %self.root,
                    "this SFTP account carries no destination id, so the marker can only be \
                     checked for presence; reconnect the account to record one"
                );
                Ok(())
            }
        }
    }

    // -- paths ---------------------------------------------------------------

    /// Resolve an object id to an absolute remote path, refusing anything that
    /// could escape the account's root.
    ///
    /// Ids are BUILT from encoded components, which can never contain `/`, `\`,
    /// `:`, a NUL, or be `.`/`..` - but an id also arrives from `file_state`,
    /// i.e. from SQLite, i.e. from a file on disk a user could edit. So the
    /// invariant is re-checked here rather than assumed. `:` is refused
    /// alongside the rest because no encoded name contains one and a colon in a
    /// path segment is illegal on a Windows-served share (and refused outright
    /// by this crate's test fixture), so seeing one means the id is corrupt.
    fn remote_path(&self, id: &str) -> anyhow::Result<String> {
        let mut out = self.root.clone();
        for segment in id.split('/') {
            if segment.is_empty() {
                continue;
            }
            if segment == "."
                || segment == ".."
                || segment.contains('\\')
                || segment.contains('\0')
                || segment.contains(':')
            {
                anyhow::bail!("sftp.id_invalid: object id {id:?} escapes the destination root");
            }
            out = join_remote(&out, segment);
        }
        Ok(out)
    }

    /// `Some(true)` for a directory, `Some(false)` for anything else that
    /// exists, `None` when the path does not exist.
    async fn stat_kind(
        sftp: &RusshSftpSession,
        path: &str,
    ) -> anyhow::Result<Option<russh_sftp::protocol::FileAttributes>> {
        match sftp.metadata(path.to_string()).await {
            Ok(attrs) => Ok(Some(attrs)),
            Err(error) if is_no_such_file(&error) => Ok(None),
            Err(error) => Err(sftp_op_error(&format!("stat {path}"), error)),
        }
    }

    /// Make sure the directory for `dir_id` exists, creating the levels below
    /// the account's root as needed.
    ///
    /// The account's own `root_path` is NEVER created: a typo there must
    /// surface as an error rather than quietly starting a backup in a brand-new
    /// directory beside the intended one. The creation probe is what proves it
    /// exists.
    ///
    /// Costs ONE round trip in the overwhelmingly common case (the directory is
    /// already there); only a miss pays for the level-by-level walk.
    async fn ensure_dir(&self, sftp: &RusshSftpSession, dir_id: &str) -> anyhow::Result<()> {
        let full = self.remote_path(dir_id)?;
        match Self::stat_kind(sftp, &full).await? {
            Some(attrs) if attrs.is_dir() => return Ok(()),
            Some(_) => anyhow::bail!(
                "sftp.not_a_directory: {full} exists on the server but is not a directory"
            ),
            None => {}
        }

        match Self::stat_kind(sftp, &self.root).await? {
            Some(attrs) if attrs.is_dir() => {}
            Some(_) => anyhow::bail!(
                "sftp.not_a_directory: the configured root path {} is not a directory",
                self.root
            ),
            None => anyhow::bail!(
                "sftp.root_missing: the configured root path {} does not exist on the server",
                self.root
            ),
        }

        let mut current = self.root.clone();
        for segment in dir_id.split('/').filter(|s| !s.is_empty()) {
            current = join_remote(&current, segment);
            match Self::stat_kind(sftp, &current).await? {
                Some(attrs) if attrs.is_dir() => continue,
                Some(_) => anyhow::bail!(
                    "sftp.not_a_directory: {current} exists on the server but is not a directory"
                ),
                None => Self::create_dir(sftp, &current).await?,
            }
        }
        Ok(())
    }

    /// `mkdir`, tolerating the directory having appeared in the meantime.
    ///
    /// SFTPv3 has no `EEXIST` status - a server reports it as the generic
    /// `SSH_FX_FAILURE` - so "already there" and "genuinely failed" are only
    /// distinguishable by looking afterwards.
    async fn create_dir(sftp: &RusshSftpSession, path: &str) -> anyhow::Result<()> {
        match sftp.create_dir(path.to_string()).await {
            Ok(()) => Ok(()),
            Err(error) => match Self::stat_kind(sftp, path).await? {
                Some(attrs) if attrs.is_dir() => Ok(()),
                _ => Err(sftp_op_error(
                    &format!("create the directory {path}"),
                    error,
                )),
            },
        }
    }

    /// Remove a remote file, treating "already gone" as success.
    async fn remove_if_present(sftp: &RusshSftpSession, path: &str) -> anyhow::Result<()> {
        match sftp.remove_file(path.to_string()).await {
            Ok(()) => Ok(()),
            Err(error) if is_no_such_file(&error) => Ok(()),
            Err(error) => Err(sftp_op_error(&format!("remove {path}"), error)),
        }
    }

    // -- sidecars ------------------------------------------------------------

    /// Read the sidecar annotating `dir_path/stored`.
    ///
    /// A missing sidecar and a corrupt one are both `Ok(None)` - the object is
    /// simply unannotated, which every caller handles. A sidecar that could not
    /// be READ for any other reason is an `Err`: silently reading a dropped
    /// connection as "no annotation" would let a collision probe hand a live
    /// object's name to a different file.
    async fn read_sidecar(
        sftp: &RusshSftpSession,
        dir_path: &str,
        stored: &str,
    ) -> anyhow::Result<Option<Sidecar>> {
        let Some(name) = meta::sidecar_name(stored) else {
            return Ok(None);
        };
        let path = join_remote(dir_path, &name);
        match sftp.read(path.clone()).await {
            Ok(raw) => Ok(meta::parse(&path, &raw)),
            Err(error) if is_no_such_file(&error) => Ok(None),
            Err(error) => Err(sftp_op_error(&format!("read the sidecar {path}"), error)),
        }
    }

    /// Write a sidecar, replacing any existing one.
    ///
    /// A plain create-truncate-write rather than the temp-and-rename dance the
    /// data path needs. The reason is that both crash windows produce the SAME
    /// observable state: a torn sidecar fails to parse and reads as
    /// "unannotated", exactly as an absent one does, and the DATA file is
    /// authoritative in both directions. Paying two extra round trips per
    /// object to move between two identical outcomes would be a cost with no
    /// benefit.
    async fn write_sidecar(
        sftp: &RusshSftpSession,
        dir_path: &str,
        sidecar: &Sidecar,
    ) -> anyhow::Result<()> {
        let Some(name) = meta::sidecar_name(&sidecar.stored) else {
            anyhow::bail!("sftp.name_invalid: a sidecar name must be a single path component");
        };
        let path = join_remote(dir_path, &name);
        let bytes = sidecar.to_bytes()?;
        let mut file = sftp
            .open_with_flags(
                path.clone(),
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| sftp_op_error(&format!("open the sidecar {path}"), e))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| sftp_io_error(&format!("write the sidecar {path}"), e))?;
        file.shutdown()
            .await
            .map_err(|e| sftp_io_error(&format!("close the sidecar {path}"), e))?;
        Ok(())
    }

    /// Remove a sidecar; a missing one - or a name that is not a single path
    /// component - is success, so the delete stays idempotent.
    async fn remove_sidecar(
        sftp: &RusshSftpSession,
        dir_path: &str,
        stored: &str,
    ) -> anyhow::Result<()> {
        let Some(name) = meta::sidecar_name(stored) else {
            return Ok(());
        };
        Self::remove_if_present(sftp, &join_remote(dir_path, &name)).await
    }

    // -- entries -------------------------------------------------------------

    /// Build a [`RemoteEntry`] from a sidecar Driven just wrote, plus the id it
    /// belongs to.
    ///
    /// Used on the commit path so a completed upload does not spend two extra
    /// round trips re-reading what it has in hand. `modified_time` is Driven's
    /// own write timestamp rather than the server's `mtime`; the two agree to
    /// within the write, and it saves a `stat`.
    fn entry_from_sidecar(sidecar: &Sidecar, id: &str, dir_id: &str) -> RemoteEntry {
        let size = sidecar.size.unwrap_or(0);
        RemoteEntry {
            id: id.to_string(),
            name: sidecar.name.clone(),
            parents: vec![dir_id.to_string()],
            size: sidecar.size,
            md5: sidecar.md5_for(size),
            mime_type: sidecar
                .mime
                .clone()
                .unwrap_or_else(|| DEFAULT_MIME.to_string()),
            modified_time: sidecar.modified_ms,
            trashed: false,
            app_properties: sidecar.props.clone(),
        }
    }

    /// Build a [`RemoteEntry`] from what the server reports, joining the
    /// sidecar on.
    ///
    /// Driven by the DATA object: an entry exists because the object exists,
    /// and the sidecar only annotates it. The reverse (list sidecars, report
    /// them as objects) would report a deleted file as live and the
    /// remote-existence audit would never re-upload it.
    fn entry_from_remote(
        id: &str,
        stored: &str,
        attrs: &russh_sftp::protocol::FileAttributes,
        sidecar: Option<Sidecar>,
    ) -> RemoteEntry {
        let dir_id = parent_of(id);
        // SFTP reports mtime in whole seconds.
        let modified_time = attrs.mtime.map(|s| i64::from(s) * 1000).unwrap_or(0);

        if attrs.is_dir() {
            return RemoteEntry {
                id: folder_prefix(id),
                name: sidecar
                    .map(|s| s.name)
                    .unwrap_or_else(|| names::decode(stored)),
                parents: vec![dir_id],
                size: None,
                md5: None,
                mime_type: FOLDER_MIME.to_string(),
                modified_time,
                trashed: false,
                app_properties: HashMap::new(),
            };
        }

        let size = attrs.len();
        RemoteEntry {
            id: id.to_string(),
            name: sidecar
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| names::decode(stored)),
            parents: vec![dir_id],
            size: Some(size),
            md5: sidecar.as_ref().and_then(|s| s.md5_for(size)),
            mime_type: sidecar
                .as_ref()
                .and_then(|s| s.mime.clone())
                .unwrap_or_else(|| DEFAULT_MIME.to_string()),
            modified_time,
            trashed: false,
            app_properties: sidecar.map(|s| s.props).unwrap_or_default(),
        }
    }

    /// Metadata for one object, by id.
    async fn entry_for(&self, sftp: &RusshSftpSession, id: &str) -> anyhow::Result<RemoteEntry> {
        let path = self.remote_path(id)?;
        let Some(attrs) = Self::stat_kind(sftp, &path).await? else {
            return Err(not_found(id));
        };
        let stored = base_name(id).to_string();
        let dir_path = self.remote_path(&parent_of(id))?;
        let sidecar = Self::read_sidecar(sftp, &dir_path, &stored).await?;
        Ok(Self::entry_from_remote(id, &stored, &attrs, sidecar))
    }

    // -- names ---------------------------------------------------------------

    /// Choose the remote filename for `original` inside `dir_id`, claiming it
    /// for the duration of the returned guard.
    ///
    /// The collision that matters is a case-insensitive or Unicode-folding
    /// remote filesystem - a Synology share, a Windows OpenSSH server, or a
    /// macOS box serving APFS. `Notes/Foo.txt` and `Notes/foo.txt` are two
    /// distinct source files that both want the same remote filename, and the
    /// naive answer - just write it - means the second silently destroys the
    /// first while every later backup reports success.
    ///
    /// The probe ASKS THE SERVER by opening `.<candidate>.driven-meta` and
    /// reading the ORIGINAL name recorded there, so it inherits the remote
    /// filesystem's own equivalence relation for free: case folding, Unicode
    /// normalization, and any locale-specific folding the server applies are
    /// all handled without Driven shipping a table.
    async fn resolve_stored_name(
        &self,
        sftp: &RusshSftpSession,
        dir_id: &str,
        original: &str,
        kind: EntryKind,
    ) -> anyhow::Result<(String, ClaimGuard)> {
        let dir_path = self.remote_path(dir_id)?;
        let encoded = names::encode(original)?;
        let candidates = [encoded.clone(), names::disambiguate(&encoded, original)];

        for candidate in candidates {
            let Some((guard, fresh)) = try_claim(&self.claims, dir_id, &candidate, original) else {
                // A different original name holds it; try the next candidate.
                continue;
            };
            if !fresh {
                // A sibling upload of the SAME original already probed this
                // name and took it; a second probe would learn nothing.
                return Ok((candidate, guard));
            }
            match Self::read_sidecar(sftp, &dir_path, &candidate).await? {
                // Ours: the same source name and the same kind. An update lands
                // on the file it already owns.
                Some(s) if s.name == original && s.kind == kind => return Ok((candidate, guard)),
                // A different original name, or the same name as the other KIND
                // (a file where we want a directory). Either way the remote
                // filesystem cannot hold both.
                Some(_) => drop(guard),
                // No sidecar. Anything sitting at that path is unowned - a data
                // file left by a create that crashed before its sidecar was
                // committed, or a file the user put there themselves. Taking it
                // is correct for the first case (the replay must land on the
                // same path) and is the only non-wedging answer for the second.
                // Unlike the local backend this does NOT spend a round trip
                // stat-ing the path merely to log a warning about it.
                None => return Ok((candidate, guard)),
            }
        }

        // Both candidates are held by other names. Reaching here needs a
        // collision in the 64-bit disambiguation digest; failing loudly beats
        // overwriting someone else's file.
        anyhow::bail!(
            "sftp.name_collision: could not find a free remote filename for {original:?} in \
             {dir_id:?}; both the encoded name and its disambiguated form are taken"
        )
    }

    // -- writing -------------------------------------------------------------

    /// Stream `body` into the remote temp path, returning `(size, md5)` over
    /// the bytes actually written.
    async fn write_temp(
        sftp: &RusshSftpSession,
        temp: &str,
        body: UploadBody,
    ) -> anyhow::Result<(u64, [u8; 16])> {
        // `SftpSession::write` opens with WRITE only and therefore CANNOT
        // create a file - the CREATE flag has to be explicit or a correct
        // server answers SSH_FX_NO_SUCH_FILE.
        let mut file = sftp
            .open_with_flags(
                temp.to_string(),
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| sftp_op_error(&format!("create {temp}"), e))?;

        let mut hasher = Md5::new();
        let mut written: u64 = 0;
        let declared = match body {
            UploadBody::Bytes(bytes) => {
                hasher.update(&bytes);
                written = bytes.len() as u64;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| sftp_io_error(&format!("write {temp}"), e))?;
                None
            }
            UploadBody::Stream { len, mut stream } => {
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    hasher.update(&chunk);
                    written += chunk.len() as u64;
                    file.write_all(&chunk)
                        .await
                        .map_err(|e| sftp_io_error(&format!("write {temp}"), e))?;
                }
                Some(len)
            }
        };
        file.shutdown()
            .await
            .map_err(|e| sftp_io_error(&format!("close {temp}"), e))?;

        // The declared length is what the executor hashed and what it will
        // verify against; a stream that produced a different number of bytes is
        // a bug we must not commit.
        if let Some(len) = declared {
            if written != len {
                anyhow::bail!(
                    "sftp.length_mismatch: the upload body declared {len} bytes but produced \
                     {written}"
                );
            }
        }
        Ok((written, hasher.finalize().into()))
    }

    /// Re-download a committed object and hash it, returning `(size, md5)`.
    ///
    /// This is step 4 of the integrity protocol in the module docs. It must
    /// stay a genuine re-read: hashing the buffer we just sent, or trusting the
    /// size the server reports, would make the executor's post-upload check
    /// compare a value against itself.
    async fn hash_remote_object(
        sftp: &RusshSftpSession,
        path: &str,
    ) -> anyhow::Result<(u64, [u8; 16])> {
        let mut file = sftp
            .open(path.to_string())
            .await
            .map_err(|e| sftp_op_error(&format!("re-open {path} to verify it"), e))?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; READ_CHUNK];
        let mut total: u64 = 0;
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| sftp_io_error(&format!("verify {path}"), e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        Ok((total, hasher.finalize().into()))
    }

    /// Publish `temp` as the object `dir_id/stored`, write its sidecar, and
    /// return the entry carrying the digest READ BACK off the server.
    #[allow(clippy::too_many_arguments)]
    async fn commit_object(
        &self,
        sftp: &RusshSftpSession,
        dir_id: &str,
        stored: &str,
        original: &str,
        mime: &str,
        props: HashMap<String, String>,
        temp: &str,
        expected: [u8; 16],
        is_create: bool,
    ) -> anyhow::Result<RemoteEntry> {
        let dir_path = self.remote_path(dir_id)?;
        let target = join_remote(&dir_path, stored);
        let id = join_id(dir_id, stored);

        // SFTPv3's rename refuses an existing destination (module docs), so the
        // target has to go first. On an update that is the normal path, not the
        // exceptional one.
        if let Err(error) = Self::remove_if_present(sftp, &target).await {
            let _ = Self::remove_if_present(sftp, temp).await;
            return Err(error);
        }
        if let Err(error) = sftp.rename(temp.to_string(), target.clone()).await {
            // The temp file is deliberately LEFT IN PLACE. The target has
            // already been removed above, so on an update this temp holds the
            // only copy of the new bytes; discarding it because the rename hit
            // ENOSPC or a permissions refusal would turn a recoverable failure
            // into data that has to be re-uploaded from the source (and, if the
            // source is gone, cannot be). It is invisible to every listing
            // (`names::TMP_PREFIX`) and the abandoned-temp sweep collects it.
            return Err(sftp_op_error(
                &format!(
                    "commit {target} (the uploaded bytes are still on the server as {temp} and \
                     were NOT discarded)"
                ),
                error,
            ));
        }

        let (size, actual) = Self::hash_remote_object(sftp, &target).await?;
        if actual != expected {
            return Err(self.checksum_mismatch(sftp, &id, is_create).await);
        }

        let sidecar = Sidecar {
            version: 1,
            kind: EntryKind::File,
            name: original.to_string(),
            stored: stored.to_string(),
            size: Some(size),
            md5: Some(hex::encode(actual)),
            mime: Some(mime.to_string()),
            modified_ms: now_ms(),
            props,
        };
        Self::write_sidecar(sftp, &dir_path, &sidecar).await?;
        Ok(Self::entry_from_sidecar(&sidecar, &id, dir_id))
    }

    /// Build the SPEC s24 `drive.checksum_mismatch` error, removing the corrupt
    /// object first when it was a CREATE.
    ///
    /// A create's target is an object that did not exist a moment ago, so
    /// deleting it restores the server to its previous state. An UPDATE's
    /// target is the user's existing backed-up file: the executor's contract is
    /// that its `file_id` is never trashed on a mismatch, and the next cycle
    /// re-writes it.
    async fn checksum_mismatch(
        &self,
        sftp: &RusshSftpSession,
        id: &str,
        is_create: bool,
    ) -> anyhow::Error {
        if !is_create {
            return anyhow::Error::new(DriveError::ChecksumMismatch {
                stranded_file_id: None,
            });
        }
        let stranded = match self.delete_object(sftp, id).await {
            Ok(()) => None,
            Err(err) => {
                tracing::error!(
                    target: crate::TARGET,
                    %id,
                    %err,
                    "could not remove the corrupt object after a checksum mismatch; keeping the \
                     op so reconcile retries the delete"
                );
                Some(id.to_string())
            }
        };
        anyhow::Error::new(DriveError::ChecksumMismatch {
            stranded_file_id: stranded,
        })
    }

    /// Remove an object and then its sidecar. Idempotent.
    ///
    /// Data first: a dangling sidecar is inert, while a live data file with no
    /// sidecar is invisible to the remote-existence audit and would be
    /// re-uploaded beside itself forever.
    async fn delete_object(&self, sftp: &RusshSftpSession, id: &str) -> anyhow::Result<()> {
        let path = self.remote_path(id)?;
        let dir_path = self.remote_path(&parent_of(id))?;
        let stored = base_name(id).to_string();

        match Self::stat_kind(sftp, &path).await? {
            Some(attrs) if attrs.is_dir() => anyhow::bail!(
                "sftp.not_a_file: refusing to delete {id:?}, which is a directory on the server"
            ),
            Some(_) => Self::remove_if_present(sftp, &path).await?,
            // Already gone. The sidecar is still swept, so a crash between the
            // two removals heals on the next attempt.
            None => {}
        }
        Self::remove_sidecar(sftp, &dir_path, &stored).await
    }

    // -- enumeration ---------------------------------------------------------

    /// Every sidecar in `dir_id` whose DATA object is still live, paired with
    /// the server's attributes for that object.
    ///
    /// One `readdir` gives both halves - the sidecars and the data entries - so
    /// the join costs no extra requests; only the sidecar CONTENTS have to be
    /// fetched, and those go out concurrently.
    ///
    /// Sidecars whose data file is gone are dropped: reporting one as live
    /// would tell the remote-existence audit that a deleted object is still on
    /// the server, and the file would never be re-uploaded.
    async fn live_annotated_files(
        &self,
        sftp: &RusshSftpSession,
        dir_id: &str,
    ) -> anyhow::Result<Vec<(Sidecar, russh_sftp::protocol::FileAttributes)>> {
        let dir_path = self.remote_path(dir_id)?;
        let entries = match sftp.read_dir(dir_path.clone()).await {
            Ok(entries) => entries,
            Err(error) if is_no_such_file(&error) => return Ok(Vec::new()),
            Err(error) => return Err(sftp_op_error(&format!("list {dir_path}"), error)),
        };

        let mut data: HashMap<String, russh_sftp::protocol::FileAttributes> = HashMap::new();
        let mut sidecar_names: Vec<String> = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            if let Some(stored) = meta::stored_from_sidecar_name(&name) {
                sidecar_names.push(stored.to_string());
            } else if !entry.metadata().is_dir() {
                data.insert(name, entry.metadata());
            }
        }

        let fetched: Vec<Option<(Sidecar, russh_sftp::protocol::FileAttributes)>> =
            futures::stream::iter(sidecar_names.into_iter().map(|stored| {
                let dir_path = dir_path.clone();
                let attrs = data.get(&stored).cloned();
                async move {
                    // A sidecar with no live data object is dangling; skip it
                    // without even reading it.
                    let Some(attrs) = attrs else { return Ok(None) };
                    let sidecar = Self::read_sidecar(sftp, &dir_path, &stored).await?;
                    Ok(sidecar
                        .filter(|s| s.kind == EntryKind::File)
                        .map(|s| (s, attrs)))
                }
            }))
            .buffered(SIDECAR_FETCH_CONCURRENCY)
            .collect::<Vec<anyhow::Result<_>>>()
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(fetched.into_iter().flatten().collect())
    }

    /// The error every method Task 4 owns returns until it lands.
    ///
    /// Deliberately an ERROR and never a plausible empty answer. The trait doc
    /// for `list_source_object_ids` spells out why: a store that answers "no
    /// live objects" is not degrading safely, it is reporting a mass deletion,
    /// and the caller would heal every recorded id by re-uploading the whole
    /// source. The same reasoning makes an invented `AboutInfo` worse than no
    /// answer.
    fn not_yet_implemented(method: &str) -> anyhow::Error {
        anyhow::Error::new(DriveError::Classified {
            kind: driven_remote::remote_store::DriveErrorClassification::Other,
            source: anyhow::anyhow!(
                "sftp.unimplemented: {method} is not implemented for the SFTP backend yet \
                 (resumable uploads, the complete source listing and quota land in the next \
                 slice); failing loudly rather than reporting an empty or invented answer"
            ),
        })
    }
}

// -- the trait ----------------------------------------------------------------

#[async_trait]
impl RemoteStore for SftpStore {
    /// `mkdir -p` plus a sidecar recording the directory's ORIGINAL name.
    ///
    /// Unlike the S3 backend (where a folder is a key prefix and needs no
    /// request) a directory must really exist before a file can be written into
    /// it. The sidecar is what lets a later `Docs` versus `docs` collision be
    /// detected on a case-insensitive remote - and what stops a directory and a
    /// file fighting over one name, which Drive permits and no filesystem does.
    ///
    /// Idempotent: an existing directory of the same original name is adopted.
    async fn ensure_folder(
        &self,
        parent_id: &str,
        name: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<RemoteEntry> {
        let channel = self.channel().await?;
        let sftp = channel.sftp();
        self.guard_root(sftp).await?;
        let dir_id = folder_prefix(parent_id);
        self.ensure_dir(sftp, &dir_id).await?;

        let (stored, _claim) = self
            .resolve_stored_name(sftp, &dir_id, name, EntryKind::Dir)
            .await?;
        let id = folder_id(&dir_id, &stored);
        self.ensure_dir(sftp, &id).await?;

        let dir_path = self.remote_path(&dir_id)?;
        let sidecar = Sidecar {
            version: 1,
            kind: EntryKind::Dir,
            name: name.to_string(),
            stored: stored.clone(),
            size: None,
            md5: None,
            mime: Some(FOLDER_MIME.to_string()),
            modified_ms: now_ms(),
            props: HashMap::new(),
        };
        Self::write_sidecar(sftp, &dir_path, &sidecar).await?;

        Ok(RemoteEntry {
            id,
            name: name.to_string(),
            parents: vec![dir_id],
            size: None,
            md5: None,
            mime_type: FOLDER_MIME.to_string(),
            modified_time: sidecar.modified_ms,
            trashed: false,
            app_properties: HashMap::new(),
        })
    }

    /// Direct children of a directory.
    ///
    /// Driven by the real directory entries, not by sidecars, so the
    /// destination picker can browse folders the user already had - and so a
    /// dangling sidecar never appears as a file. Driven's own control entries
    /// (sidecars, in-flight temp files, macOS AppleDouble shadows) are filtered
    /// out; `names::encode` guarantees no user object can be hidden by that
    /// filter.
    async fn list_folder(
        &self,
        folder_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<Vec<RemoteEntry>> {
        let channel = self.channel().await?;
        let sftp = channel.sftp();
        let dir_id = folder_prefix(folder_id);
        let dir_path = self.remote_path(&dir_id)?;

        let entries = match sftp.read_dir(dir_path.clone()).await {
            Ok(entries) => entries,
            Err(error) if is_no_such_file(&error) => return Ok(Vec::new()),
            Err(error) => return Err(sftp_op_error(&format!("list {dir_path}"), error)),
        };

        // `russh-sftp`'s `ReadDir` iterator filters `.` and `..` for us; a walk
        // built on raw `readdir` packets would see them and have to skip them
        // itself.
        let children: Vec<(String, russh_sftp::protocol::FileAttributes)> = entries
            .filter(|entry| !names::is_reserved_control_name(&entry.file_name()))
            .map(|entry| (entry.file_name(), entry.metadata()))
            .collect();

        futures::stream::iter(children.into_iter().map(|(stored, attrs)| {
            let dir_path = dir_path.clone();
            let dir_id = dir_id.clone();
            async move {
                let sidecar = Self::read_sidecar(sftp, &dir_path, &stored).await?;
                let id = if attrs.is_dir() {
                    // Fully qualified: the trait names this method's parameter
                    // `folder_id` too, which shadows the free function.
                    crate::store::folder_id(&dir_id, &stored)
                } else {
                    join_id(&dir_id, &stored)
                };
                Ok(Self::entry_from_remote(&id, &stored, &attrs, sidecar))
            }
        }))
        .buffered(SIDECAR_FETCH_CONCURRENCY)
        .collect::<Vec<anyhow::Result<RemoteEntry>>>()
        .await
        .into_iter()
        .collect()
    }

    /// Write a new object at `<parent_id>/<encoded name>`.
    ///
    /// Unlike Drive, a filesystem cannot hold two files of one name in one
    /// directory: a `create` over an existing path OVERWRITES rather than
    /// producing a duplicate. That is strictly safer than the semantics the
    /// trait documents, and the caller-side "do not create over an existing
    /// `file_state.drive_file_id`" discipline still holds.
    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let channel = self.channel().await?;
        let sftp = channel.sftp();
        self.guard_root(sftp).await?;
        let dir_id = folder_prefix(parent_id);
        self.ensure_dir(sftp, &dir_id).await?;

        let (stored, _claim) = self
            .resolve_stored_name(sftp, &dir_id, name, EntryKind::File)
            .await?;

        let dir_path = self.remote_path(&dir_id)?;
        let temp = join_remote(&dir_path, &names::temp_name());
        let (_, expected) = match Self::write_temp(sftp, &temp, body).await {
            Ok(v) => v,
            Err(error) => {
                let _ = Self::remove_if_present(sftp, &temp).await;
                return Err(error);
            }
        };
        self.commit_object(
            sftp,
            &dir_id,
            &stored,
            name,
            mime,
            app_properties,
            &temp,
            expected,
            true,
        )
        .await
    }

    /// Overwrite the object at `file_id` (which IS its root-relative path).
    ///
    /// The existing sidecar is read first so the original name, MIME type and
    /// the rest of the identity stamp are carried forward - a patch that names
    /// one property must not drop the others.
    ///
    /// ## A missing target is NOT an error here
    ///
    /// On Drive a `file_id` is an opaque handle: once the object is gone the id
    /// can never be revived, which is why the executor has a dedicated
    /// `update_target_is_missing` self-heal. A remote path is not opaque - it is
    /// derived from the relative path, and writing to it revives it - so an
    /// update against a deleted object correctly RE-CREATES it at exactly the
    /// path a re-plan would have chosen, leaving `file_state.drive_file_id`
    /// valid.
    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let channel = self.channel().await?;
        let sftp = channel.sftp();
        self.guard_root(sftp).await?;
        let dir_id = parent_of(file_id);
        let stored = base_name(file_id).to_string();
        if stored.is_empty() {
            anyhow::bail!("sftp.id_invalid: {file_id:?} does not name an object");
        }
        // Validate the id BEFORE any request, so a tampered `file_state` row
        // cannot reach the wire at all.
        let dir_path = self.remote_path(&dir_id)?;
        let _ = self.remote_path(file_id)?;
        self.ensure_dir(sftp, &dir_id).await?;

        let existing = Self::read_sidecar(sftp, &dir_path, &stored).await?;
        let original = existing
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| names::decode(&stored));
        let mime = existing
            .as_ref()
            .and_then(|s| s.mime.clone())
            .unwrap_or_else(|| DEFAULT_MIME.to_string());
        let mut props = existing.map(|s| s.props).unwrap_or_default();
        props.extend(app_properties_patch);

        // The caller carried the id, so it already owns this name; re-probing
        // could hand it away while this write is in flight.
        let _claim = claim_exact(&self.claims, &dir_id, &stored, &original);

        let temp = join_remote(&dir_path, &names::temp_name());
        let (_, expected) = match Self::write_temp(sftp, &temp, body).await {
            Ok(v) => v,
            Err(error) => {
                let _ = Self::remove_if_present(sftp, &temp).await;
                return Err(error);
            }
        };
        self.commit_object(
            sftp, &dir_id, &stored, &original, &mime, props, &temp, expected, false,
        )
        .await
    }

    async fn resumable_session(
        &self,
        _kind: ResumableKind,
        _mime: &str,
        _size: u64,
    ) -> anyhow::Result<ResumableSession> {
        Err(Self::not_yet_implemented("resumable_session"))
    }

    async fn resume_chunk(
        &self,
        _session: &ResumableSession,
        _offset: u64,
        _chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        Err(Self::not_yet_implemented("resume_chunk"))
    }

    /// SSH has NO trash: this permanently deletes the object, exactly like
    /// [`Self::delete_permanent`]. Driven deliberately does not simulate a
    /// trash by moving objects aside - nothing would ever empty it, and a
    /// backup destination that only grows fills the disk it lives on. A missing
    /// object is success (idempotent).
    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        let channel = self.channel().await?;
        self.guard_root(channel.sftp()).await?;
        self.delete_object(channel.sftp(), file_id).await
    }

    async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
        let channel = self.channel().await?;
        self.guard_root(channel.sftp()).await?;
        self.delete_object(channel.sftp(), file_id).await
    }

    /// Metadata for one object.
    ///
    /// `md5` comes from the sidecar, and only when the sidecar's recorded size
    /// still matches the object's - see [`Sidecar::md5_for`] for why a mismatch
    /// reports nothing rather than a stale digest.
    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        let channel = self.channel().await?;
        self.entry_for(channel.sftp(), file_id).await
    }

    /// Open a streaming download.
    ///
    /// The returned handle owns its own reference to the SFTP channel, so it
    /// outlives the borrow taken here and a long restore does not hold the
    /// store's session lock. If the transport dies mid-download the reads fail,
    /// which is the honest outcome - the restore sink retries.
    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        let channel = self.channel().await?;
        let path = self.remote_path(file_id)?;
        match channel.sftp().open(path.clone()).await {
            Ok(file) => Ok(DownloadStream(Box::new(file))),
            Err(error) if is_no_such_file(&error) => Err(not_found(file_id)),
            Err(error) => Err(sftp_op_error(&format!("open {path}"), error)),
        }
    }

    /// Find an object under `parent_id` carrying `op_uuid`.
    ///
    /// One directory's sidecars are read; there is no index to consult and no
    /// listing to page. Scope keeps it affordable - reconciliation calls this
    /// for a single crashed op, on a single directory - and the reads go out
    /// concurrently, so a folder of N objects costs roughly `N /
    /// SIDECAR_FETCH_CONCURRENCY` round trips rather than N.
    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        let channel = self.channel().await?;
        let dir_id = folder_prefix(parent_id);
        let mut matches: Vec<(Sidecar, russh_sftp::protocol::FileAttributes)> = self
            .live_annotated_files(channel.sftp(), &dir_id)
            .await?
            .into_iter()
            .filter(|(s, _)| {
                s.props
                    .get(driven_remote::props::CLIENT_OP_UUID_KEY)
                    .is_some_and(|v| v == op_uuid)
            })
            .collect();
        if matches.len() > 1 {
            tracing::warn!(
                target: crate::TARGET,
                count = matches.len(),
                "multiple objects carry the same client op uuid; adopting the most recent"
            );
            matches.sort_by_key(|(s, _)| s.modified_ms);
        }
        Ok(matches.pop().map(|(sidecar, attrs)| {
            let id = join_id(&dir_id, &sidecar.stored);
            let stored = sidecar.stored.clone();
            Self::entry_from_remote(&id, &stored, &attrs, Some(sidecar))
        }))
    }

    async fn list_source_object_ids(
        &self,
        _source_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<HashSet<String>> {
        Err(Self::not_yet_implemented("list_source_object_ids"))
    }

    async fn about(&self) -> anyhow::Result<AboutInfo> {
        Err(Self::not_yet_implemented("about"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SftpAuthKind;
    use crate::test_support::TestSftpServer;
    use driven_remote::remote_store::DriveErrorClassification;

    /// Initialize `root` as a Driven destination and return its id.
    ///
    /// Stands in for Task 6's creation probe, which is what writes the marker
    /// for real - the same role `driven_localfs::prepare_destination` plays for
    /// that crate's `store_in` test helper.
    fn seed_destination(root: &std::path::Path) -> String {
        let destination_id = uuid::Uuid::new_v4().to_string();
        let marker = crate::config::DestinationMarker::new(&destination_id, 1_700_000_000_000);
        std::fs::write(
            root.join(names::MARKER_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        destination_id
    }

    fn store_for(server: &TestSftpServer) -> SftpStore {
        let destination_id = seed_destination(server.root());
        let mut config = server.pinned_config(SftpAuthKind::Password);
        config.destination_id = Some(destination_id);
        SftpStore::new(&config, &server.password_credential())
            .expect("a valid config builds a store")
    }

    /// A store whose root has NOT been initialized - the shape of `root_path`
    /// pointing at a directory of the user's own data.
    fn unprepared_store_for(server: &TestSftpServer) -> SftpStore {
        SftpStore::new(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .expect("a valid config builds a store")
    }

    fn body(bytes: &'static [u8]) -> UploadBody {
        UploadBody::Bytes(Bytes::from_static(bytes))
    }

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn digest(bytes: &[u8]) -> [u8; 16] {
        let mut h = Md5::new();
        h.update(bytes);
        h.finalize().into()
    }

    async fn download_to_vec(store: &SftpStore, id: &str) -> Vec<u8> {
        let mut stream = store.download(id).await.expect("download");
        let mut out = Vec::new();
        stream.0.read_to_end(&mut out).await.expect("read");
        out
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
        assert_eq!(parent_of("d/e/a.txt"), "d/e/");
        assert_eq!(parent_of("a.txt"), "");
        assert_eq!(parent_of(""), "");
    }

    #[test]
    fn a_root_path_and_an_id_join_without_doubling_a_slash() {
        let mut config = SftpConfig {
            host: "example.com".to_string(),
            port: 22,
            root_path: "/".to_string(),
            username: "u".to_string(),
            auth: SftpAuthKind::Password,
            host_key_fingerprint: Some("SHA256:x".to_string()),
            destination_id: None,
        };
        let cred = SftpCredential::Password {
            password: "p".to_string(),
        };
        let store = SftpStore::new(&config, &cred).unwrap();
        assert_eq!(store.remote_path("").unwrap(), "/");
        assert_eq!(store.remote_path("a/b.txt").unwrap(), "/a/b.txt");
        assert_eq!(store.remote_path("a/").unwrap(), "/a");

        config.root_path = "/backups/driven".to_string();
        let store = SftpStore::new(&config, &cred).unwrap();
        assert_eq!(store.remote_path("").unwrap(), "/backups/driven");
        assert_eq!(
            store.remote_path("Docs/a.txt").unwrap(),
            "/backups/driven/Docs/a.txt"
        );
    }

    #[test]
    fn an_id_can_never_escape_the_configured_root() {
        let store = SftpStore::new(
            &SftpConfig {
                host: "example.com".to_string(),
                port: 22,
                root_path: "/backups".to_string(),
                username: "u".to_string(),
                auth: SftpAuthKind::Password,
                host_key_fingerprint: Some("SHA256:x".to_string()),
                destination_id: None,
            },
            &SftpCredential::Password {
                password: "p".to_string(),
            },
        )
        .unwrap();
        for evil in [
            "../etc/passwd",
            "a/../../etc",
            "a/./b",
            "a\\b",
            "a\0b",
            "C:/Windows",
        ] {
            assert!(
                store.remote_path(evil).is_err(),
                "{evil:?} must be refused, not resolved"
            );
        }
    }

    #[test]
    fn a_kex_algorithm_mismatch_is_recognisable_in_a_connect_failure() {
        // russh 0.62.5 `negotiation.rs` phrasing, which the session layer wraps
        // into a Network-classified `Connect`. Recognising it is what lets the
        // store log that the retries are futile.
        assert!(looks_like_algorithm_incompatibility(
            "sftp.connect_failed: the SSH handshake failed: No common HostKey algorithm - \
             ours: [\"ssh-ed25519\"], theirs: [\"ssh-rsa\"]"
        ));
        assert!(looks_like_algorithm_incompatibility(
            "sftp.connect_failed: the SSH handshake failed: Key exchange failed"
        ));
        assert!(!looks_like_algorithm_incompatibility(
            "sftp.connect_failed: connecting to nas.local:22: Connection refused"
        ));
    }

    #[tokio::test]
    async fn create_then_metadata_round_trips_the_bytes_and_the_app_properties() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let stamped = props(&[
            (driven_remote::props::SOURCE_ID_KEY, "src-1"),
            (driven_remote::props::CLIENT_OP_UUID_KEY, "op-1"),
            ("driven.relative_path_hash", "deadbeef"),
        ]);
        let entry = store
            .create(
                "",
                "report.txt",
                "text/plain",
                body(b"hello"),
                stamped.clone(),
            )
            .await
            .expect("create");

        assert_eq!(entry.id, "report.txt");
        assert_eq!(entry.name, "report.txt");
        assert_eq!(entry.size, Some(5));
        assert_eq!(entry.mime_type, "text/plain");
        assert_eq!(entry.app_properties, stamped);
        assert_eq!(
            entry.md5,
            Some(digest(b"hello")),
            "the digest must be the one read BACK off the server"
        );

        // The object really is on the server, under the name Driven reported.
        assert_eq!(
            std::fs::read(server.root().join("report.txt")).unwrap(),
            b"hello"
        );

        let fetched = store.metadata("report.txt").await.expect("metadata");
        assert_eq!(fetched.id, entry.id);
        assert_eq!(fetched.name, "report.txt");
        assert_eq!(fetched.size, Some(5));
        assert_eq!(fetched.mime_type, "text/plain");
        assert_eq!(fetched.md5, Some(digest(b"hello")));
        assert_eq!(fetched.app_properties, stamped);
    }

    #[tokio::test]
    async fn metadata_for_an_object_that_is_not_there_is_a_not_found_answer() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        let error = store
            .metadata("nope.txt")
            .await
            .expect_err("no such object");
        assert!(format!("{error:?}").contains("sftp.not_found"), "{error:?}");
        assert!(store.download("nope.txt").await.is_err());
    }

    #[tokio::test]
    async fn folders_are_created_once_adopted_on_a_second_call_and_hold_objects() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let folder = store
            .ensure_folder("", "Docs", &DriveContext::MyDrive)
            .await
            .expect("ensure_folder");
        assert_eq!(folder.id, "Docs/");
        assert_eq!(folder.name, "Docs");
        assert_eq!(folder.mime_type, FOLDER_MIME);
        assert!(folder.size.is_none());
        assert!(server.root().join("Docs").is_dir());

        // Idempotent: the same name adopts the same directory rather than
        // making `Docs~<digest>` beside it.
        let again = store
            .ensure_folder("", "Docs", &DriveContext::MyDrive)
            .await
            .expect("ensure_folder is idempotent");
        assert_eq!(again.id, folder.id);

        let nested = store
            .ensure_folder("Docs/", "Reports 2026", &DriveContext::MyDrive)
            .await
            .expect("nested folder");
        assert_eq!(nested.id, "Docs/Reports 2026/");

        let entry = store
            .create(
                &nested.id,
                "q1.csv",
                "text/csv",
                body(b"a,b,c"),
                HashMap::new(),
            )
            .await
            .expect("create inside a nested folder");
        assert_eq!(entry.id, "Docs/Reports 2026/q1.csv");
        assert_eq!(
            std::fs::read(server.root().join("Docs/Reports 2026/q1.csv")).unwrap(),
            b"a,b,c"
        );

        let listed = store
            .list_folder("Docs/", &DriveContext::MyDrive)
            .await
            .expect("list");
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Reports 2026"], "{names:?}");
        assert_eq!(listed[0].id, "Docs/Reports 2026/");
        assert_eq!(listed[0].mime_type, FOLDER_MIME);
    }

    #[tokio::test]
    async fn update_rewrites_in_place_and_carries_the_identity_stamp_forward() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let created = store
            .create(
                "",
                "notes.md",
                "text/markdown",
                body(b"first"),
                props(&[(driven_remote::props::SOURCE_ID_KEY, "src-1")]),
            )
            .await
            .unwrap();

        // The rename-into-place path has to remove the existing object first -
        // SFTPv3 has no clobbering rename - so an update over a live object is
        // exactly the case that would fail if that step were missing.
        let updated = store
            .update(
                &created.id,
                body(b"second and longer"),
                props(&[(driven_remote::props::CLIENT_OP_UUID_KEY, "op-2")]),
            )
            .await
            .expect("update");

        assert_eq!(updated.id, created.id, "an update must not move the object");
        assert_eq!(updated.size, Some(17));
        assert_eq!(updated.md5, Some(digest(b"second and longer")));
        assert_eq!(
            updated.mime_type, "text/markdown",
            "the MIME type survives an update that does not restate it"
        );
        assert_eq!(updated.name, "notes.md");
        // The patch is merged onto the existing stamp, not substituted for it.
        assert_eq!(
            updated
                .app_properties
                .get(driven_remote::props::SOURCE_ID_KEY),
            Some(&"src-1".to_string())
        );
        assert_eq!(
            updated
                .app_properties
                .get(driven_remote::props::CLIENT_OP_UUID_KEY),
            Some(&"op-2".to_string())
        );
        assert_eq!(
            std::fs::read(server.root().join("notes.md")).unwrap(),
            b"second and longer"
        );
        // Exactly one object plus its sidecar - the update did not leave the
        // previous copy or a temp file behind.
        let listed = store.list_folder("", &DriveContext::MyDrive).await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
    }

    #[tokio::test]
    async fn a_streamed_body_lands_byte_identical_and_downloads_back() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        // Larger than one SFTP packet, so the write and the verify read both
        // have to loop.
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let chunks: Vec<anyhow::Result<Bytes>> = payload
            .chunks(64 * 1024)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let entry = store
            .create(
                "",
                "big.bin",
                "application/octet-stream",
                UploadBody::Stream {
                    len: payload.len() as u64,
                    stream: Box::new(futures::stream::iter(chunks)),
                },
                HashMap::new(),
            )
            .await
            .expect("streamed create");

        assert_eq!(entry.size, Some(payload.len() as u64));
        assert_eq!(entry.md5, Some(digest(&payload)));
        assert_eq!(download_to_vec(&store, &entry.id).await, payload);
    }

    #[tokio::test]
    async fn a_stream_that_lies_about_its_length_is_never_committed() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        let error = store
            .create(
                "",
                "short.bin",
                "application/octet-stream",
                UploadBody::Stream {
                    len: 100,
                    stream: Box::new(futures::stream::iter(vec![Ok(Bytes::from_static(
                        b"only 6",
                    ))])),
                },
                HashMap::new(),
            )
            .await
            .expect_err("a short stream must not commit");
        assert!(
            format!("{error:?}").contains("sftp.length_mismatch"),
            "{error:?}"
        );
        assert!(
            store.metadata("short.bin").await.is_err(),
            "nothing may have been published"
        );
        // And the temp file it was writing into is cleaned up - nothing but the
        // destination marker is left behind.
        let leftovers: Vec<String> = std::fs::read_dir(server.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != names::MARKER_FILE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn an_in_flight_temp_file_and_the_sidecars_are_invisible_to_a_listing() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        store
            .create("", "real.txt", "text/plain", body(b"hi"), HashMap::new())
            .await
            .unwrap();

        // A leftover temp file from an interrupted upload, exactly as
        // `names::temp_name` would have named it.
        std::fs::write(server.root().join(names::temp_name()), b"partial").unwrap();
        // And a macOS AppleDouble shadow, which a Mac mounting the same share
        // writes beside every object.
        std::fs::write(server.root().join("._real.txt"), b"xattrs").unwrap();

        // The sidecar and the destination marker are genuinely on the server...
        assert!(server.root().join(".real.txt.driven-meta").is_file());
        assert!(server.root().join(names::MARKER_FILE).is_file());
        // ...and none of the four appear as objects.
        let listed = store.list_folder("", &DriveContext::MyDrive).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["real.txt"], "{names:?}");
    }

    #[tokio::test]
    async fn filesystem_hostile_names_round_trip() {
        // Every one of these is legal on ext4/APFS and is rejected or mangled
        // by an NTFS or exFAT share - which is exactly why the encoding exists,
        // and why an SFTP destination needs it as much as a USB stick does.
        //
        // `a:b.txt` is deliberately ABSENT despite being the canonical example:
        // the test fixture refuses `:` in a path segment (stricter than sshd on
        // purpose), so including it would fail with a PERMISSION_DENIED that
        // was fixture strictness rather than a backend bug. `names.rs` covers
        // the colon case directly.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        let hostile = [
            "a?b|c.txt",
            "star*.txt",
            "quote\".txt",
            "angle<>.txt",
            "back\\slash.txt",
            "trailing dot.",
            "trailing space ",
            "100% done.txt",
            "Ünïcödé \u{1f600}.txt",
            // Looks exactly like a macOS AppleDouble shadow. It must survive as
            // the user's own file rather than being filtered out as noise.
            "._notes.txt",
            // Looks exactly like one of Driven's own control files.
            "notes.driven-meta",
            names::MARKER_FILE,
            // MS-DOS device names. Not merely rejected on a Windows OpenSSH
            // server - `nul.txt` OPENS THE NULL DEVICE, so the write succeeds
            // and the bytes vanish.
            "CON",
            "nul.txt",
            "COM1.log",
        ];

        let mut ids = Vec::new();
        for (i, name) in hostile.iter().enumerate() {
            let bytes: Vec<u8> = (0..(512 + i)).map(|b| (b % 251) as u8).collect();
            let entry = store
                .create(
                    "",
                    name,
                    "application/octet-stream",
                    UploadBody::Bytes(Bytes::from(bytes.clone())),
                    HashMap::new(),
                )
                .await
                .unwrap_or_else(|e| panic!("create {name:?}: {e:?}"));
            assert_eq!(entry.name, *name, "the ORIGINAL name must be reported back");
            assert_eq!(entry.md5, Some(digest(&bytes)), "{name:?}");
            assert_eq!(download_to_vec(&store, &entry.id).await, bytes, "{name:?}");
            ids.push(entry.id);
        }

        // Every one is a DISTINCT object, and every one is browsable - the
        // failure that matters is a hostile name that is written but then
        // invisible to the listing and the audit.
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), hostile.len(), "{ids:?}");
        let listed = store.list_folder("", &DriveContext::MyDrive).await.unwrap();
        let mut listed_names: Vec<String> = listed.iter().map(|e| e.name.clone()).collect();
        listed_names.sort();
        let mut expected: Vec<String> = hostile.iter().map(|n| n.to_string()).collect();
        expected.sort();
        assert_eq!(listed_names, expected);
    }

    #[tokio::test]
    async fn very_long_names_stay_distinct_and_restorable() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        // A shared 280-character prefix differing only in the last character -
        // the worst case for a truncating scheme, and comfortably past the
        // 255-byte single-component limit every filesystem enforces.
        let base = "e".repeat(280);
        let a_name = format!("{base}A");
        let b_name = format!("{base}B");
        let a_bytes = vec![1u8; 4096];
        let b_bytes = vec![2u8; 5000];

        let a = store
            .create(
                "",
                &a_name,
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(a_bytes.clone())),
                HashMap::new(),
            )
            .await
            .expect("create the first long name");
        let b = store
            .create(
                "",
                &b_name,
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(b_bytes.clone())),
                HashMap::new(),
            )
            .await
            .expect("create the second long name");

        assert_ne!(a.id, b.id, "two long names must not collapse onto one file");
        assert_eq!(download_to_vec(&store, &a.id).await, a_bytes);
        assert_eq!(download_to_vec(&store, &b.id).await, b_bytes);
        // The ORIGINAL name still round-trips through the sidecar, even though
        // the remote filename is truncated - which is what makes a restore able
        // to put the file back under its real name.
        assert_eq!(a.name, a_name);
        assert_eq!(b.name, b_name);
        // The sidecar's own filename has to fit too: it is the stored name plus
        // a leading dot and the 12-byte suffix.
        assert!(
            base_name(&a.id).len() + 1 + names::META_SUFFIX.len() <= 255,
            "{}",
            base_name(&a.id).len()
        );

        // Re-creating the same long name must land on the SAME object (an
        // overwrite), not accrete one copy per attempt.
        let again = store
            .create(
                "",
                &a_name,
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(vec![3u8; 32])),
                HashMap::new(),
            )
            .await
            .expect("re-create the first long name");
        assert_eq!(again.id, a.id);
        assert_eq!(
            store
                .list_folder("", &DriveContext::MyDrive)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_user_file_named_like_a_sidecar_survives_a_full_round_trip() {
        // The hazard of the flat sidecar namespace: without the encoding, this
        // file would be filtered out of every listing as if it were Driven's
        // own metadata - backed up once and invisible from then on.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let entry = store
            .create(
                "",
                "notes.driven-meta",
                "text/plain",
                body(b"a user's own file"),
                HashMap::new(),
            )
            .await
            .unwrap();
        assert_ne!(entry.id, "notes.driven-meta", "the name must be escaped");
        assert_eq!(entry.name, "notes.driven-meta", "but reported verbatim");

        let listed = store.list_folder("", &DriveContext::MyDrive).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["notes.driven-meta"], "{names:?}");
        assert_eq!(
            download_to_vec(&store, &entry.id).await,
            b"a user's own file"
        );
    }

    #[tokio::test]
    async fn two_source_names_never_share_one_remote_object() {
        // On a case-INSENSITIVE remote (a Synology share, Windows OpenSSH, a
        // default APFS volume) `Foo.txt` and `foo.txt` want the same filename,
        // and writing both would silently destroy one. On a case-SENSITIVE one
        // there is no collision to detect and both are stored verbatim, which
        // is also correct. Either way the two must not be one object.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let a = store
            .create("", "Foo.txt", "text/plain", body(b"upper"), HashMap::new())
            .await
            .unwrap();
        let b = store
            .create("", "foo.txt", "text/plain", body(b"lower"), HashMap::new())
            .await
            .unwrap();

        assert_ne!(a.id, b.id, "two source names must not share one object");
        assert_eq!(download_to_vec(&store, &a.id).await, b"upper");
        assert_eq!(download_to_vec(&store, &b.id).await, b"lower");
        assert_eq!(a.name, "Foo.txt");
        assert_eq!(b.name, "foo.txt");
    }

    #[tokio::test]
    async fn trashing_removes_the_object_and_its_sidecar_and_is_idempotent() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        let entry = store
            .create("", "gone.txt", "text/plain", body(b"bye"), HashMap::new())
            .await
            .unwrap();

        assert!(server.root().join("gone.txt").is_file());
        assert!(server.root().join(".gone.txt.driven-meta").is_file());

        store.trash(&entry.id).await.expect("trash");
        assert!(!server.root().join("gone.txt").exists());
        assert!(
            !server.root().join(".gone.txt.driven-meta").exists(),
            "a dangling sidecar would make a deleted object look live to the audit"
        );
        assert!(store.metadata(&entry.id).await.is_err());

        // Trash is a permanent delete here, and both spellings are idempotent.
        store.trash(&entry.id).await.expect("trashing twice");
        store
            .delete_permanent(&entry.id)
            .await
            .expect("delete_permanent on a missing object");
        assert!(store
            .list_folder("", &DriveContext::MyDrive)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn trashing_refuses_a_directory_rather_than_guessing() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        let folder = store
            .ensure_folder("", "Docs", &DriveContext::MyDrive)
            .await
            .unwrap();
        let error = store.trash(&folder.id).await.expect_err("not a file");
        assert!(
            format!("{error:?}").contains("sftp.not_a_file"),
            "{error:?}"
        );
        assert!(server.root().join("Docs").is_dir());
    }

    #[tokio::test]
    async fn find_by_op_uuid_adopts_an_object_a_crash_orphaned() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let orphan = store
            .create(
                "",
                "orphan.txt",
                "text/plain",
                body(b"adopt me"),
                props(&[
                    (driven_remote::props::SOURCE_ID_KEY, "src-1"),
                    (driven_remote::props::CLIENT_OP_UUID_KEY, "op-crashed"),
                ]),
            )
            .await
            .unwrap();
        store
            .create(
                "",
                "other.txt",
                "text/plain",
                body(b"unrelated"),
                props(&[(driven_remote::props::CLIENT_OP_UUID_KEY, "op-other")]),
            )
            .await
            .unwrap();

        let found = store
            .find_by_op_uuid("", "op-crashed", &DriveContext::MyDrive)
            .await
            .expect("find")
            .expect("the orphan is adopted");
        assert_eq!(found.id, orphan.id);
        assert_eq!(found.name, "orphan.txt");
        assert_eq!(found.size, Some(8));
        assert_eq!(
            found
                .app_properties
                .get(driven_remote::props::SOURCE_ID_KEY),
            Some(&"src-1".to_string())
        );

        assert!(store
            .find_by_op_uuid("", "op-never-happened", &DriveContext::MyDrive)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_dangling_sidecar_is_never_adopted() {
        // The state a crash between the two removals leaves: the sidecar is
        // still there and the DATA object is gone. Adopting it would hand the
        // executor a `file_id` pointing at nothing.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        store
            .create(
                "",
                "ghost.txt",
                "text/plain",
                body(b"boo"),
                props(&[(driven_remote::props::CLIENT_OP_UUID_KEY, "op-ghost")]),
            )
            .await
            .unwrap();

        std::fs::remove_file(server.root().join("ghost.txt")).unwrap();
        assert!(server.root().join(".ghost.txt.driven-meta").is_file());

        assert!(store
            .find_by_op_uuid("", "op-ghost", &DriveContext::MyDrive)
            .await
            .unwrap()
            .is_none());
        assert!(
            store
                .list_folder("", &DriveContext::MyDrive)
                .await
                .unwrap()
                .is_empty(),
            "a sidecar is not an object"
        );
    }

    #[tokio::test]
    async fn a_non_root_root_path_scopes_everything_under_it() {
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::create_dir(server.root().join("backups")).unwrap();
        let destination_id = seed_destination(&server.root().join("backups"));
        let mut config = server.pinned_config(SftpAuthKind::Password);
        config.root_path = "/backups".to_string();
        config.destination_id = Some(destination_id);
        let store = SftpStore::new(&config, &server.password_credential()).unwrap();

        let entry = store
            .create(
                "",
                "scoped.txt",
                "text/plain",
                body(b"in here"),
                HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(entry.id, "scoped.txt", "ids stay relative to the root");
        assert!(server.root().join("backups/scoped.txt").is_file());
        assert!(
            !server.root().join("scoped.txt").exists(),
            "nothing may land above the configured root"
        );
        assert_eq!(download_to_vec(&store, &entry.id).await, b"in here");
    }

    #[tokio::test]
    async fn a_missing_root_path_is_reported_rather_than_created() {
        // A typo in `root_path` must not quietly start a backup in a brand-new
        // directory beside the intended one, where the user would never find it.
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.pinned_config(SftpAuthKind::Password);
        config.root_path = "/not-there".to_string();
        let store = SftpStore::new(&config, &server.password_credential()).unwrap();

        let error = store
            .create("", "a.txt", "text/plain", body(b"x"), HashMap::new())
            .await
            .expect_err("the root must exist");
        assert!(
            format!("{error:?}").contains("sftp.root_missing"),
            "{error:?}"
        );
        // A wrong root_path and an uninitialized one must not read the same:
        // the user actions are "fix the path" and "reconnect the account".
        assert!(
            !format!("{error:?}").contains("sftp.dest_marker_missing"),
            "{error:?}"
        );
        assert!(!server.root().join("not-there").exists());
    }

    #[tokio::test]
    async fn no_mutating_operation_will_touch_a_root_that_is_not_a_driven_destination() {
        // The hazard: `root_path` is a string the user typed, and a directory
        // of their OWN data looks exactly like an initialized-but-empty
        // destination. Aimed there, `resolve_stored_name`'s adopt-unannotated
        // path and `commit_object`'s remove-then-rename would DESTROY the
        // same-named files sitting in it. The marker is what makes that
        // impossible - and its absence must fail closed on every write.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = unprepared_store_for(&server);
        // The user's own file, with a name a backup could plausibly collide on.
        std::fs::write(server.root().join("report.txt"), b"the user's own data").unwrap();

        let mut errors = vec![
            store
                .create(
                    "",
                    "report.txt",
                    "text/plain",
                    body(b"a backup"),
                    HashMap::new(),
                )
                .await
                .expect_err("create must refuse"),
            store
                .update("report.txt", body(b"a backup"), HashMap::new())
                .await
                .expect_err("update must refuse"),
            store
                .ensure_folder("", "Docs", &DriveContext::MyDrive)
                .await
                .expect_err("ensure_folder must refuse"),
            store
                .trash("report.txt")
                .await
                .expect_err("trash must refuse"),
            store
                .delete_permanent("report.txt")
                .await
                .expect_err("delete_permanent must refuse"),
        ];
        for error in errors.drain(..) {
            let chain = format!("{error:?}");
            assert!(chain.contains("sftp.dest_marker_missing"), "{chain}");
            // The `sftp.` code rides on top of the taxonomy the app already
            // knows how to act on, rather than replacing it.
            assert!(chain.contains("drive.dest_folder_missing"), "{chain}");
            assert_eq!(
                driven_remote::classification_of(&error),
                Some(DriveErrorClassification::Other),
                "the anyhow context must not hide the DriveError from the breaker: {chain}"
            );
        }

        // The canary: the user's file is untouched and nothing was created.
        assert_eq!(
            std::fs::read(server.root().join("report.txt")).unwrap(),
            b"the user's own data"
        );
        let leftovers: Vec<String> = std::fs::read_dir(server.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["report.txt".to_string()], "{leftovers:?}");
    }

    #[tokio::test]
    async fn a_root_holding_a_different_driven_destination_is_refused() {
        // Two machines pointed at one shared `root_path` on a NAS. Each writes
        // its own marker; the second overwrites the first, and the first must
        // then refuse rather than interleave two trees in one directory.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        store
            .create("", "mine.txt", "text/plain", body(b"mine"), HashMap::new())
            .await
            .expect("the destination is this account's");

        // The other machine re-initializes the same directory.
        seed_destination(server.root());

        let error = store
            .create("", "later.txt", "text/plain", body(b"mine"), HashMap::new())
            .await
            .expect_err("a different destination id must be refused");
        let chain = format!("{error:?}");
        assert!(chain.contains("sftp.dest_marker_mismatch"), "{chain}");
        assert!(chain.contains("drive.dest_folder_missing"), "{chain}");
        assert!(!server.root().join("later.txt").exists());
    }

    #[tokio::test]
    async fn an_account_with_no_destination_id_still_gets_the_presence_check() {
        // An account written before `destination_id` existed. The identity half
        // of the check is unavailable, but the half that stops the destructive
        // cases - "is this a Driven destination at all" - must still hold.
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.pinned_config(SftpAuthKind::Password);
        config.destination_id = None;
        let store = SftpStore::new(&config, &server.password_credential()).unwrap();

        let error = store
            .create("", "a.txt", "text/plain", body(b"x"), HashMap::new())
            .await
            .expect_err("no marker, no write");
        assert!(
            format!("{error:?}").contains("sftp.dest_marker_missing"),
            "{error:?}"
        );

        seed_destination(server.root());
        store
            .create("", "a.txt", "text/plain", body(b"x"), HashMap::new())
            .await
            .expect("any marker satisfies an account that carries no id");
    }

    #[tokio::test]
    async fn a_marker_written_by_the_local_folder_backend_is_accepted_here() {
        // The two backends share a marker filename AND a marker schema so one
        // backup tree stays interchangeable - a user can copy a USB stick onto
        // a NAS. A comment claiming that is not a check.
        let server = TestSftpServer::spawn().await.unwrap();
        let localfs_marker = driven_localfs::DestinationMarker::new("shared-destination", 42);
        std::fs::write(
            server.root().join(names::MARKER_FILE),
            serde_json::to_vec(&localfs_marker).unwrap(),
        )
        .unwrap();

        let mut config = server.pinned_config(SftpAuthKind::Password);
        config.destination_id = Some("shared-destination".to_string());
        let store = SftpStore::new(&config, &server.password_credential()).unwrap();
        store
            .create("", "a.txt", "text/plain", body(b"x"), HashMap::new())
            .await
            .expect("the local backend's marker must satisfy this one");
    }

    #[tokio::test]
    async fn a_corrupt_marker_fails_closed() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        std::fs::write(server.root().join(names::MARKER_FILE), b"{ truncated").unwrap();

        let error = store
            .create("", "a.txt", "text/plain", body(b"x"), HashMap::new())
            .await
            .expect_err("an unreadable marker cannot prove anything");
        assert!(
            format!("{error:?}").contains("sftp.dest_marker_unreadable"),
            "{error:?}"
        );
        assert!(!server.root().join("a.txt").exists());
    }

    #[tokio::test]
    async fn reads_are_deliberately_not_marker_gated() {
        // Unlike the local backend - which gates every method, because its
        // marker is written by `prepare_destination` BEFORE any store can be
        // constructed - the SFTP account-creation flow mirrors
        // `create_s3_account`, whose probe calls `list_folder` on a store built
        // from unpersisted config against a server that holds nothing yet
        // (src-tauri/src/commands/accounts.rs). Gating reads would make that
        // probe fail on a fresh, perfectly good server. The hazard the marker
        // exists for is entirely write-side, so leaving reads open costs
        // nothing: a read cannot destroy the user's data.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = unprepared_store_for(&server);
        std::fs::write(server.root().join("theirs.txt"), b"not ours").unwrap();

        let listed = store
            .list_folder("", &DriveContext::MyDrive)
            .await
            .expect("browsing an uninitialized server is how the picker works");
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["theirs.txt"], "{names:?}");
        assert!(store.metadata("theirs.txt").await.is_ok());
        assert!(store
            .find_by_op_uuid("", "op-1", &DriveContext::MyDrive)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn an_object_id_from_a_tampered_state_db_cannot_write_outside_the_root() {
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        let error = store
            .update("../escaped.txt", body(b"x"), HashMap::new())
            .await
            .expect_err("a hand-edited id must be refused");
        assert!(
            format!("{error:?}").contains("sftp.id_invalid"),
            "{error:?}"
        );
        assert!(!server.root().parent().unwrap().join("escaped.txt").exists());
    }

    #[tokio::test]
    async fn the_session_is_established_lazily_and_survives_a_drop() {
        // Building a store must not require the box to be awake, and an
        // operation after the transport dies must reconnect rather than fail.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);
        assert!(
            store.session.read().await.is_none(),
            "constructing a store must not open a connection"
        );

        store
            .create("", "a.txt", "text/plain", body(b"one"), HashMap::new())
            .await
            .unwrap();
        assert!(store.session.read().await.is_some());

        {
            let mut guard = store.session.write().await;
            guard.as_mut().unwrap().disconnect_for_test().await;
        }
        for _ in 0..200 {
            if !store
                .session
                .read()
                .await
                .as_ref()
                .is_some_and(|s| s.is_connected())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let entry = store
            .create("", "b.txt", "text/plain", body(b"two"), HashMap::new())
            .await
            .expect("the store reconnects transparently");
        assert_eq!(download_to_vec(&store, &entry.id).await, b"two");
    }

    #[tokio::test]
    async fn the_methods_the_next_slice_owns_fail_loudly_rather_than_inventing_an_answer() {
        // `list_source_object_ids` in particular: the trait spells out that an
        // empty answer is NOT a safe degradation - the caller reads it as a
        // mass deletion and re-uploads the whole source.
        let server = TestSftpServer::spawn().await.unwrap();
        let store = store_for(&server);

        let error = store
            .list_source_object_ids("src-1", &DriveContext::MyDrive)
            .await
            .expect_err("an empty live set would read as a mass deletion");
        assert!(
            format!("{error:?}").contains("sftp.unimplemented"),
            "{error:?}"
        );
        assert_eq!(
            driven_remote::classification_of(&error),
            Some(DriveErrorClassification::Other),
            "fatal for this op, never a silent success"
        );

        assert!(store.about().await.is_err());
        assert!(store
            .resumable_session(
                ResumableKind::Update {
                    file_id: "a.txt".to_string()
                },
                "text/plain",
                1,
            )
            .await
            .is_err());
    }
}
