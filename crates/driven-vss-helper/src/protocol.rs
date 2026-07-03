//! The wire protocol between the un-elevated app and the elevated VSS helper
//! (DESIGN s5.3.1).
//!
//! # Framing
//!
//! Every message is a single frame: `[kind: u8][len: u32 big-endian][payload]`.
//! - `kind = 0` ([`KIND_CONTROL`]): the payload is a JSON-encoded [`Control`]
//!   message. Control frames are small and length-capped at
//!   [`MAX_CONTROL_FRAME`].
//! - `kind = 1` ([`KIND_DATA`]): the payload is a raw chunk of file bytes
//!   streamed from the shadow copy - NO base64 overhead - length-capped at
//!   [`MAX_DATA_FRAME`].
//!
//! The framing is pure `std::io::{Read, Write}`, so both the elevated server
//! (over a `std::fs::File` wrapping the pipe HANDLE) and the un-elevated client
//! (over a `std::fs::File` opened on `\\.\pipe\...`) use the exact same code,
//! and the round-trip is unit-tested cross-OS over an in-memory buffer.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The protocol version the client and server must agree on in the handshake.
/// Bumped on any incompatible change to [`Control`] or the framing.
pub const PROTOCOL_VERSION: u32 = 1;

/// Frame kind: a JSON-encoded [`Control`] message.
pub const KIND_CONTROL: u8 = 0;
/// Frame kind: a raw chunk of streamed file bytes.
pub const KIND_DATA: u8 = 1;

/// Maximum control-frame payload length (64 KiB). A control message is small
/// (a path plus a few scalars); the cap bounds a malicious/garbled peer's
/// per-frame allocation.
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;

/// Maximum data-frame payload length (1 MiB). This is also the largest chunk
/// the server will stream per [`Control::Read`], so a client's `max_len` is
/// clamped to it.
pub const MAX_DATA_FRAME: usize = 1024 * 1024;

/// A control message. Serialised as JSON in a [`KIND_CONTROL`] frame.
///
/// The vocabulary is deliberately tiny (DESIGN s5.3.1): a version handshake,
/// open-a-locked-file, pull-a-chunk, close-the-file, release-the-cycle, and
/// shut-down. Anything the helper does not recognise deserialises to an error
/// at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    /// Client -> server: open the handshake, declaring the client's protocol
    /// version.
    Hello {
        /// The [`PROTOCOL_VERSION`] the client speaks.
        protocol_version: u32,
    },
    /// Server -> client: accept the handshake, echoing the agreed version.
    HelloOk {
        /// The [`PROTOCOL_VERSION`] the server speaks.
        protocol_version: u32,
    },
    /// Client -> server: create/reuse the shadow copy for `volume` and open the
    /// locked file at `live_path` (validated to be under an allowed root).
    OpenLocked {
        /// The volume label to snapshot (`"C:"`).
        volume: String,
        /// The absolute live path of the locked file to read via the snapshot.
        live_path: String,
    },
    /// Server -> client: the file opened; its total size in bytes follows via
    /// [`Control::Read`] pulls.
    OpenOk {
        /// The opened file's size in bytes.
        size: u64,
    },
    /// Client -> server: pull up to `max_len` more bytes of the open file. The
    /// reply is a single [`KIND_DATA`] frame (empty payload = EOF).
    Read {
        /// The maximum chunk size the client wants (clamped to
        /// [`MAX_DATA_FRAME`] server-side).
        max_len: u32,
    },
    /// Client -> server: close the currently-open file (its snapshot stays
    /// cached for the cycle). Reply: [`Control::Ok`].
    CloseFile,
    /// Client -> server: release every snapshot created this cycle (the helper
    /// keeps running for the next cycle). Reply: [`Control::Ok`].
    EndCycle,
    /// Client -> server: release everything and exit the helper. Reply:
    /// [`Control::Ok`], after which the server process terminates.
    Shutdown,
    /// Server -> client: the previous request succeeded (no payload).
    Ok,
    /// Server -> client: the previous request failed. `code` is a stable machine
    /// token (e.g. `invalid_request`, `not_allowed`, `vss_unavailable`,
    /// `io_error`); `message` is a short, secret-free human string.
    Error {
        /// Stable machine-readable error token.
        code: String,
        /// Short, secret-free human-readable detail.
        message: String,
    },
}

/// A decoded frame: either a control message or a raw data chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A [`KIND_CONTROL`] frame.
    Control(Control),
    /// A [`KIND_DATA`] frame (raw file bytes; empty = EOF).
    Data(Vec<u8>),
}

/// Write a control message as a [`KIND_CONTROL`] frame.
pub fn write_control<W: Write>(w: &mut W, msg: &Control) -> io::Result<()> {
    let body = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("encode control: {e}")))?;
    if body.len() > MAX_CONTROL_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame exceeds cap",
        ));
    }
    write_frame(w, KIND_CONTROL, &body)
}

/// Write a raw data chunk as a [`KIND_DATA`] frame. `data` must not exceed
/// [`MAX_DATA_FRAME`].
pub fn write_data<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_DATA_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "data frame exceeds cap",
        ));
    }
    write_frame(w, KIND_DATA, data)
}

/// Low-level: write `[kind][len:u32 BE][payload]`.
fn write_frame<W: Write>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))?;
    w.write_all(&[kind])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame, enforcing the per-kind length caps. An unknown kind or an
/// over-cap length is an [`io::ErrorKind::InvalidData`] error (the boundary
/// rejects garbage rather than allocating on a peer's say-so).
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_be_bytes(len_bytes) as usize;

    let cap = match kind[0] {
        KIND_CONTROL => MAX_CONTROL_FRAME,
        KIND_DATA => MAX_DATA_FRAME,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame kind {other}"),
            ))
        }
    };
    if len > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame length exceeds cap",
        ));
    }

    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;

    match kind[0] {
        KIND_CONTROL => {
            let msg: Control = serde_json::from_slice(&payload).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("decode control: {e}"))
            })?;
            Ok(Frame::Control(msg))
        }
        KIND_DATA => Ok(Frame::Data(payload)),
        // Unreachable: caps match filtered kinds above.
        _ => unreachable!(),
    }
}

/// Read a frame and require it to be a control message (a data frame where a
/// control was expected is a protocol error).
pub fn read_control<R: Read>(r: &mut R) -> io::Result<Control> {
    match read_frame(r)? {
        Frame::Control(c) => Ok(c),
        Frame::Data(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a control frame, got data",
        )),
    }
}

/// Read a frame and require it to be a data chunk (a control message where data
/// was expected is a protocol error). Returns the raw bytes (empty = EOF).
pub fn read_data<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    match read_frame(r)? {
        Frame::Data(d) => Ok(d),
        Frame::Control(c) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected a data frame, got control {c:?}"),
        )),
    }
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
            Control::OpenLocked {
                volume: "C:".into(),
                live_path: r"C:\Users\me\Outlook.pst".into(),
            },
            Control::OpenOk { size: 4096 },
            Control::Read { max_len: 65536 },
            Control::CloseFile,
            Control::EndCycle,
            Control::Shutdown,
            Control::Ok,
            Control::Error {
                code: "not_allowed".into(),
                message: "path outside configured roots".into(),
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
    fn data_frames_carry_raw_bytes_and_eof_is_empty() {
        let mut buf = Vec::new();
        let chunk = vec![0xABu8; 1000];
        write_data(&mut buf, &chunk).unwrap();
        write_data(&mut buf, &[]).unwrap(); // EOF marker
        let mut cur = Cursor::new(buf);
        assert_eq!(read_data(&mut cur).unwrap(), chunk);
        assert!(read_data(&mut cur).unwrap().is_empty());
    }

    #[test]
    fn read_control_rejects_a_data_frame() {
        let mut buf = Vec::new();
        write_data(&mut buf, b"payload").unwrap();
        let mut cur = Cursor::new(buf);
        let err = read_control(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_data_rejects_a_control_frame() {
        let mut buf = Vec::new();
        write_control(&mut buf, &Control::Ok).unwrap();
        let mut cur = Cursor::new(buf);
        let err = read_data(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversize_data_write_is_rejected() {
        let mut buf = Vec::new();
        let too_big = vec![0u8; MAX_DATA_FRAME + 1];
        let err = write_data(&mut buf, &too_big).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reader_rejects_unknown_kind() {
        // Hand-craft a frame with an invalid kind byte.
        let mut buf = Vec::new();
        buf.push(0x7F); // bogus kind
        buf.extend_from_slice(&0u32.to_be_bytes());
        let mut cur = Cursor::new(buf);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reader_rejects_oversize_declared_length() {
        // A control frame whose declared length exceeds the cap must be
        // rejected BEFORE any large allocation.
        let mut buf = Vec::new();
        buf.push(KIND_CONTROL);
        buf.extend_from_slice(&((MAX_CONTROL_FRAME as u32) + 1).to_be_bytes());
        let mut cur = Cursor::new(buf);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
