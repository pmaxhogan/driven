//! Mapping SSH/SFTP failures onto Driven's SPEC s24 error taxonomy.
//!
//! This module classifies a small, protocol-version-neutral [`SftpFailure`]
//! summary rather than a `russh`/`russh-sftp` error type directly. The
//! [`session`](crate::session) layer's only job is an honest translation of
//! whatever those libraries hand back onto [`SftpFailure`] before calling
//! [`sftp_error`] - so the classification table itself stays independent of
//! any one library version, and is fully tested on its own.
//!
//! The split (design doc, "Error mapping"):
//!
//! | Cause | Classification | Why |
//! |---|---|---|
//! | TCP connect refused / unreachable / timed out | [`DriveErrorClassification::Network`] | Retryable with backoff; the box may come back. |
//! | An established connection drops mid-operation (reset, EOF, `SSH_FX_NO_CONNECTION` / `SSH_FX_CONNECTION_LOST`) | [`DriveErrorClassification::Network`] | Same shape as a connect failure - the pipe went away, not a decision the server made. |
//! | SSH authentication rejected (bad password / key / passphrase) | [`DriveErrorClassification::AuthInvalidGrant`] | The account needs new credentials; moves to `needs_reauth`. |
//! | The server's host key does not match the pinned fingerprint | [`DriveErrorClassification::AuthInvalidGrant`] | Treated like an auth failure: hard-fail and surface an attention state, never silently retried (a swapped host key could mean a MITM). |
//! | The account's config carries no pinned fingerprint at all | [`DriveErrorClassification::AuthInvalidGrant`] | Same bucket, different cause: an unpinned account can only be repaired by re-running the creation/reconnect probe, which is exactly what `AuthInvalidGrant` routes to. |
//! | `SSH_FXP_STATUS` with an ENOSPC-shaped message (`SSH_FX_FAILURE` on a write, "no space left", "disk full", ...) | [`DriveErrorClassification::StorageQuota`] | The remote destination is full. |
//! | `SSH_FX_FAILURE` with no ENOSPC signature | [`DriveErrorClassification::Transient5xx`] | A generic server-side failure (busy resource, transient lock); worth a retry. |
//! | Any other `SSH_FXP_STATUS` code | [`DriveErrorClassification::Other`] | Fatal for this op (e.g. `SSH_FX_BAD_MESSAGE`, `SSH_FX_OP_UNSUPPORTED`, a permission refusal on the object itself). |

use driven_remote::remote_store::DriveErrorClassification;
use driven_remote::DriveError;

/// SFTPv3 `SSH_FXP_STATUS` codes (`draft-ietf-secsh-filexfer-02` s7), which
/// every server implements regardless of which later protocol version it
/// negotiates. Declared here rather than depending on `russh-sftp` because
/// this crate does not depend on it yet.
pub mod status_code {
    /// `SSH_FX_OK`.
    pub const OK: u32 = 0;
    /// `SSH_FX_EOF`.
    pub const EOF: u32 = 1;
    /// `SSH_FX_NO_SUCH_FILE`.
    pub const NO_SUCH_FILE: u32 = 2;
    /// `SSH_FX_PERMISSION_DENIED`.
    pub const PERMISSION_DENIED: u32 = 3;
    /// `SSH_FX_FAILURE` - a generic, otherwise-unclassified failure.
    pub const FAILURE: u32 = 4;
    /// `SSH_FX_BAD_MESSAGE` - a protocol framing error.
    pub const BAD_MESSAGE: u32 = 5;
    /// `SSH_FX_NO_CONNECTION` (pseudo-code some clients synthesize).
    pub const NO_CONNECTION: u32 = 6;
    /// `SSH_FX_CONNECTION_LOST` (pseudo-code some clients synthesize).
    pub const CONNECTION_LOST: u32 = 7;
    /// `SSH_FX_OP_UNSUPPORTED`.
    pub const OP_UNSUPPORTED: u32 = 8;
}

/// A coarse, protocol-version-neutral summary of an SSH/SFTP failure.
///
/// This is the seam between the (not-yet-written) `russh`/`russh-sftp`
/// session layer and the classification table: the session layer's only job
/// is to produce an honest [`SftpFailure`] from whatever error type that
/// library version hands back.
#[derive(Debug, Clone)]
pub enum SftpFailure {
    /// The initial TCP connect failed or timed out (refused, unreachable, no
    /// route, connect timeout).
    Connect {
        /// A human-readable detail for the error chain.
        detail: String,
    },
    /// An established connection was lost mid-operation (reset, EOF, an i/o
    /// timeout on a live session).
    ConnectionLost {
        /// A human-readable detail for the error chain.
        detail: String,
    },
    /// SSH authentication was rejected (bad password, bad key, bad
    /// passphrase, or the server refused every offered method).
    AuthFailed {
        /// A human-readable detail for the error chain.
        detail: String,
    },
    /// The server's host key fingerprint did not match the pinned one.
    HostKeyMismatch {
        /// A human-readable detail for the error chain (e.g. the expected vs.
        /// observed fingerprint).
        detail: String,
    },
    /// The account's [`SftpConfig`](crate::SftpConfig) carries no pinned
    /// host-key fingerprint, so there is nothing to verify the server against.
    ///
    /// This is a *precondition* failure, not a network outcome:
    /// [`SftpSession::connect`](crate::session::SftpSession::connect) refuses
    /// to open the socket at all. An unpinned config is a transient
    /// account-creation state that only
    /// [`connect_and_pin`](crate::session::SftpSession::connect_and_pin) is
    /// allowed to observe.
    HostKeyUnpinned {
        /// A human-readable detail for the error chain (e.g. the host it would
        /// have connected to).
        detail: String,
    },
    /// An `SSH_FXP_STATUS` reply from the SFTP subsystem.
    Status {
        /// The raw status code (see [`status_code`]).
        code: u32,
        /// The status message text the server sent.
        message: String,
    },
}

/// Substrings (checked case-insensitively) that mark an `SSH_FX_FAILURE`
/// message as "the destination is out of space" rather than some other
/// generic failure. SFTP has no dedicated ENOSPC status code - servers report
/// a full disk as `SSH_FX_FAILURE` with a human-readable message, so the
/// message text is the only signal available.
const ENOSPC_MARKERS: &[&str] = &[
    "no space",
    "enospc",
    "disk full",
    "out of space",
    "not enough space",
    "quota exceeded",
];

/// Does this `SSH_FXP_STATUS` message look like "the remote disk is full"?
fn is_enospc_shaped(message: &str) -> bool {
    let lower = message.to_lowercase();
    ENOSPC_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Classify an [`SftpFailure`] into the pacer / circuit-breaker verdict.
pub fn sftp_error_classification(failure: &SftpFailure) -> DriveErrorClassification {
    match failure {
        SftpFailure::Connect { .. } | SftpFailure::ConnectionLost { .. } => {
            DriveErrorClassification::Network
        }
        SftpFailure::AuthFailed { .. }
        | SftpFailure::HostKeyMismatch { .. }
        | SftpFailure::HostKeyUnpinned { .. } => DriveErrorClassification::AuthInvalidGrant,
        SftpFailure::Status { code, message } => classify_status(*code, message),
    }
}

/// Classify an `SSH_FXP_STATUS` code + message per the table in the module
/// docs.
fn classify_status(code: u32, message: &str) -> DriveErrorClassification {
    if is_enospc_shaped(message) {
        return DriveErrorClassification::StorageQuota;
    }
    match code {
        status_code::NO_CONNECTION | status_code::CONNECTION_LOST => {
            DriveErrorClassification::Network
        }
        status_code::FAILURE => DriveErrorClassification::Transient5xx,
        _ => DriveErrorClassification::Other,
    }
}

/// Build the classified [`DriveError`] for an [`SftpFailure`], embedding a
/// SPEC-s24-style dotted code in the message so the error reads consistently
/// with the rest of the app's error taxonomy.
pub fn sftp_error(failure: SftpFailure) -> DriveError {
    let kind = sftp_error_classification(&failure);
    let message = match &failure {
        SftpFailure::Connect { detail } => format!("sftp.connect_failed: {detail}"),
        SftpFailure::ConnectionLost { detail } => format!("sftp.connection_lost: {detail}"),
        SftpFailure::AuthFailed { detail } => format!("sftp.auth_failed: {detail}"),
        SftpFailure::HostKeyMismatch { detail } => format!("sftp.host_key_mismatch: {detail}"),
        SftpFailure::HostKeyUnpinned { detail } => format!("sftp.host_key_unpinned: {detail}"),
        SftpFailure::Status { code, message } => format!("sftp.status_{code}: {message}"),
    };
    DriveError::Classified {
        kind,
        source: anyhow::anyhow!(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_failures_and_connection_loss_classify_as_network() {
        for failure in [
            SftpFailure::Connect {
                detail: "connection refused".to_string(),
            },
            SftpFailure::Connect {
                detail: "connect timed out".to_string(),
            },
            SftpFailure::ConnectionLost {
                detail: "connection reset by peer".to_string(),
            },
        ] {
            assert_eq!(
                sftp_error_classification(&failure),
                DriveErrorClassification::Network,
                "{failure:?}"
            );
        }
    }

    #[test]
    fn auth_and_host_key_failures_classify_as_auth_invalid_grant() {
        for failure in [
            SftpFailure::AuthFailed {
                detail: "all authentication methods failed".to_string(),
            },
            SftpFailure::HostKeyMismatch {
                detail: "expected SHA256:abc, got SHA256:def".to_string(),
            },
            SftpFailure::HostKeyUnpinned {
                detail: "no pinned fingerprint for nas.example:22".to_string(),
            },
        ] {
            assert_eq!(
                sftp_error_classification(&failure),
                DriveErrorClassification::AuthInvalidGrant,
                "{failure:?}"
            );
        }
    }

    #[test]
    fn enospc_shaped_failure_messages_classify_as_storage_quota() {
        for message in [
            "No space left on device",
            "write failed: ENOSPC",
            "disk full",
            "quota exceeded for user",
        ] {
            let failure = SftpFailure::Status {
                code: status_code::FAILURE,
                message: message.to_string(),
            };
            assert_eq!(
                sftp_error_classification(&failure),
                DriveErrorClassification::StorageQuota,
                "{message:?}"
            );
        }
    }

    #[test]
    fn a_generic_failure_status_is_transient() {
        let failure = SftpFailure::Status {
            code: status_code::FAILURE,
            message: "resource temporarily busy".to_string(),
        };
        assert_eq!(
            sftp_error_classification(&failure),
            DriveErrorClassification::Transient5xx
        );
    }

    #[test]
    fn connection_status_codes_classify_as_network() {
        for code in [status_code::NO_CONNECTION, status_code::CONNECTION_LOST] {
            let failure = SftpFailure::Status {
                code,
                message: "the connection is gone".to_string(),
            };
            assert_eq!(
                sftp_error_classification(&failure),
                DriveErrorClassification::Network
            );
        }
    }

    #[test]
    fn other_status_codes_are_fatal_for_this_op() {
        for code in [
            status_code::NO_SUCH_FILE,
            status_code::PERMISSION_DENIED,
            status_code::BAD_MESSAGE,
            status_code::OP_UNSUPPORTED,
        ] {
            let failure = SftpFailure::Status {
                code,
                message: "unclassified".to_string(),
            };
            assert_eq!(
                sftp_error_classification(&failure),
                DriveErrorClassification::Other,
                "code {code}"
            );
        }
    }

    #[test]
    fn sftp_error_embeds_a_dotted_code_and_the_right_classification() {
        let e = sftp_error(SftpFailure::AuthFailed {
            detail: "bad password".to_string(),
        });
        assert_eq!(
            e.classification(),
            DriveErrorClassification::AuthInvalidGrant
        );
        assert!(e.to_string().contains("auth.invalid_grant"));
        let chain = format!("{e:?}");
        assert!(chain.contains("sftp.auth_failed"));
        assert!(chain.contains("bad password"));
    }

    #[test]
    fn the_local_status_constants_match_the_wire_codes_russh_sftp_sends() {
        // `status_code` is declared locally (module docs explain why), but the
        // session layer feeds it `russh_sftp`'s `StatusCode` discriminants. If
        // the two ever drift, every classification below would silently be
        // wired to the wrong number - so assert the mapping rather than
        // assuming it.
        use russh_sftp::protocol::StatusCode;
        for (code, constant) in [
            (StatusCode::Ok, status_code::OK),
            (StatusCode::Eof, status_code::EOF),
            (StatusCode::NoSuchFile, status_code::NO_SUCH_FILE),
            (StatusCode::PermissionDenied, status_code::PERMISSION_DENIED),
            (StatusCode::Failure, status_code::FAILURE),
            (StatusCode::BadMessage, status_code::BAD_MESSAGE),
            (StatusCode::NoConnection, status_code::NO_CONNECTION),
            (StatusCode::ConnectionLost, status_code::CONNECTION_LOST),
            (StatusCode::OpUnsupported, status_code::OP_UNSUPPORTED),
        ] {
            assert_eq!(code as u32, constant, "{code:?}");
        }
    }

    #[test]
    fn every_classification_this_module_can_produce_is_exercised() {
        // A guard against silently dropping a class from the mapping table:
        // one representative failure per DriveErrorClassification this
        // module is documented to produce.
        let cases: &[(SftpFailure, DriveErrorClassification)] = &[
            (
                SftpFailure::Connect {
                    detail: "refused".to_string(),
                },
                DriveErrorClassification::Network,
            ),
            (
                SftpFailure::AuthFailed {
                    detail: "bad key".to_string(),
                },
                DriveErrorClassification::AuthInvalidGrant,
            ),
            (
                SftpFailure::Status {
                    code: status_code::FAILURE,
                    message: "no space left on device".to_string(),
                },
                DriveErrorClassification::StorageQuota,
            ),
            (
                SftpFailure::Status {
                    code: status_code::FAILURE,
                    message: "busy".to_string(),
                },
                DriveErrorClassification::Transient5xx,
            ),
            (
                SftpFailure::Status {
                    code: status_code::BAD_MESSAGE,
                    message: "malformed packet".to_string(),
                },
                DriveErrorClassification::Other,
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(&sftp_error_classification(failure), expected, "{failure:?}");
        }
    }
}
