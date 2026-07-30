//! The wire protocol between the un-elevated app and the root mount broker
//! (DESIGN s5.3.2).
//!
//! # Framing
//!
//! Every message is a single frame: `[len: u32 big-endian][payload]`, where
//! the payload is a JSON-encoded [`Control`] message, length-capped at
//! [`MAX_CONTROL_FRAME`]. Unlike the Windows VSS protocol there is NO data
//! frame kind: the broker never streams file bytes (the app reads the mounted
//! snapshot directly with its own uid), so the vocabulary is control-only.
//!
//! # Deliberately absent: snapshot deletion
//!
//! An earlier revision had a `DeleteSnapshot` verb. It was REMOVED after
//! measuring that `tmutil deletelocalsnapshots` succeeds unprivileged: keeping
//! it would have meant carrying a client-supplied string into a root process's
//! argv for no functional gain. Deletion is now a plain unprivileged call in
//! [`crate::snapshot`]. Every verb the broker still accepts is one that
//! genuinely requires root.
//!
//! The framing is pure `std::io::{Read, Write}` so both ends share the code
//! and the round-trip is unit-tested cross-OS over an in-memory buffer.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The protocol version the client and broker must agree on in the handshake.
/// Bumped on any incompatible change to [`Control`] or the framing.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum control-frame payload length (64 KiB). A control message is small
/// (a path plus a few scalars); the cap bounds a malicious/garbled peer's
/// per-frame allocation.
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;

/// A control message. Serialised as JSON in a length-prefixed frame.
///
/// The vocabulary is deliberately tiny (DESIGN s5.3.2): a version handshake,
/// mount-a-snapshot, unmount-everything, and shut-down. Deletion is NOT a verb
/// here - see the module docs above.
/// Anything the broker does not recognise deserialises to an error at the
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    /// Client -> broker: open the handshake, declaring the client's protocol
    /// version.
    Hello {
        /// The [`PROTOCOL_VERSION`] the client speaks.
        protocol_version: u32,
    },
    /// Broker -> client: accept the handshake, echoing the agreed version.
    HelloOk {
        /// The [`PROTOCOL_VERSION`] the broker speaks.
        protocol_version: u32,
    },
    /// Client -> broker: mount the named APFS local snapshot of `volume_mount`
    /// read-only at a broker-owned private mountpoint.
    ///
    /// `volume_mount` must be one of the allow-listed volume mount points
    /// fixed on the broker's command line at launch; `snapshot_name` must
    /// match the strict `com.apple.TimeMachine.<date>.local` shape (both are
    /// re-validated broker-side from scratch - the client is untrusted).
    MountSnapshot {
        /// The live volume mount point the snapshot belongs to (e.g. `/` or
        /// `/System/Volumes/Data`).
        volume_mount: String,
        /// The APFS local snapshot name, e.g.
        /// `com.apple.TimeMachine.2026-07-29-154532.local`.
        snapshot_name: String,
    },
    /// Broker -> client: the snapshot is mounted; read it at `mountpoint`.
    MountOk {
        /// The read-only mountpoint the snapshot is exposed at.
        mountpoint: String,
    },
    /// Client -> broker: unmount every snapshot mount this broker created
    /// (idempotent; used at end-of-cycle). Reply: [`Control::Ok`].
    UnmountAll,
    /// Client -> broker: unmount everything and exit the broker. Reply:
    /// [`Control::Ok`], after which the broker process terminates.
    Shutdown,
    /// Broker -> client: the previous request succeeded (no payload).
    Ok,
    /// Broker -> client: the previous request failed. `code` is a stable
    /// machine token (e.g. `invalid_request`, `not_allowed`, `mount_failed`,
    /// `io_error`); `message` is a short, secret-free human string.
    Error {
        /// Stable machine-readable error token.
        code: String,
        /// Short, secret-free human-readable detail.
        message: String,
    },
}

/// Write a control message as a length-prefixed frame.
pub fn write_control<W: Write>(w: &mut W, msg: &Control) -> io::Result<()> {
    let body = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("encode control: {e}")))?;
    if body.len() > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame exceeds cap",
        ));
    }
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one control message, enforcing the length cap BEFORE allocating (the
/// boundary rejects garbage rather than allocating on a peer's say-so).
pub fn read_control<R: Read>(r: &mut R) -> io::Result<Control> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds cap",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    let msg: Control = serde_json::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("decode control: {e}")))?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn control_round_trips_through_a_buffer() {
        let msgs = [
            Control::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            Control::HelloOk {
                protocol_version: PROTOCOL_VERSION,
            },
            Control::MountSnapshot {
                volume_mount: "/System/Volumes/Data".into(),
                snapshot_name: "com.apple.TimeMachine.2026-07-29-154532.local".into(),
            },
            Control::MountOk {
                mountpoint: "/private/var/run/driven-apfs-1234/m0".into(),
            },
            Control::UnmountAll,
            Control::Shutdown,
            Control::Ok,
            Control::Error {
                code: "not_allowed".into(),
                message: "volume not in the launch allow-list".into(),
            },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_control(&mut buf, m).unwrap();
        }
        let mut cur = Cursor::new(buf);
        for m in &msgs {
            assert_eq!(&read_control(&mut cur).unwrap(), m);
        }
    }

    #[test]
    fn reader_rejects_oversize_declared_length() {
        // A frame whose declared length exceeds the cap must be rejected
        // BEFORE any large allocation.
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_CONTROL_FRAME as u32) + 1).to_be_bytes());
        let mut cur = Cursor::new(buf);
        let err = read_control(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reader_rejects_garbage_json() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u32.to_be_bytes());
        buf.extend_from_slice(b"{{{{");
        let mut cur = Cursor::new(buf);
        let err = read_control(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_hang() {
        let mut full = Vec::new();
        write_control(&mut full, &Control::Ok).unwrap();
        // Drop the last byte: read_exact must surface UnexpectedEof.
        let truncated = &full[..full.len() - 1];
        let mut cur = Cursor::new(truncated.to_vec());
        let err = read_control(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
