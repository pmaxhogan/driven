//! The un-elevated app's client for the root mount broker.
//!
//! # Server authentication (the app trusts nobody either)
//!
//! The socket lives in a directory the APP owns, so a hostile same-uid
//! process could remove the app's socket and squat its own in that path.
//! Before speaking, the client therefore checks `getpeereid` on the CONNECTED
//! stream: a unix socket's peer credentials are the binding process's, and
//! only root can be the genuine broker - a same-uid squatter cannot present
//! uid 0. This is the TOCTOU-free unix analogue of the Windows client's
//! verify-the-server-exe check (DESIGN s5.3.1).

#![cfg(target_os = "macos")]

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::protocol::{read_control, write_control, Control, PROTOCOL_VERSION};

/// A connected, handshaken broker client. One per app session (the provider
/// serialises requests; the vocabulary is tiny and each request round-trips).
#[derive(Debug)]
pub struct HelperClient {
    stream: UnixStream,
}

/// A broker-reported request failure (stable code + secret-free message).
#[derive(Debug, thiserror::Error)]
#[error("broker error [{code}]: {message}")]
pub struct BrokerError {
    /// Stable machine-readable token from the broker.
    pub code: String,
    /// Short human-readable detail.
    pub message: String,
}

/// Client-side errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Socket-level I/O failed (broker not up yet, died, protocol garbage).
    #[error("broker I/O: {0}")]
    Io(#[from] io::Error),
    /// The peer is not root - a squatter, not the broker. NEVER retried.
    #[error("socket peer is not root; refusing to speak")]
    NotRoot,
    /// The broker rejected the request.
    #[error(transparent)]
    Broker(#[from] BrokerError),
    /// The broker replied out-of-vocabulary.
    #[error("unexpected broker reply")]
    UnexpectedReply,
}

impl HelperClient {
    /// Connect to the broker at `socket`, verify the peer is root, and
    /// perform the version handshake. Fails fast (bounded timeouts) so the
    /// provider's Pending path never blocks a backup worker for long.
    pub fn connect(socket: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;

        // Server auth: the binder must be root.
        {
            use std::os::fd::AsRawFd;
            let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (u32::MAX, u32::MAX);
            // SAFETY: valid connected fd; valid out-params.
            if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
                return Err(ClientError::Io(io::Error::last_os_error()));
            }
            if uid != 0 {
                return Err(ClientError::NotRoot);
            }
        }

        let mut client = Self { stream };
        client.send(&Control::Hello {
            protocol_version: PROTOCOL_VERSION,
        })?;
        match client.recv()? {
            Control::HelloOk { .. } => Ok(client),
            Control::Error { code, message } => Err(BrokerError { code, message }.into()),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// Mount `snapshot_name` of `volume_mount`; returns the read-only
    /// mountpoint. Mount reuse is broker-side, so calling twice is cheap.
    pub fn mount_snapshot(
        &mut self,
        volume_mount: &str,
        snapshot_name: &str,
    ) -> Result<String, ClientError> {
        self.send(&Control::MountSnapshot {
            volume_mount: volume_mount.to_string(),
            snapshot_name: snapshot_name.to_string(),
        })?;
        match self.recv()? {
            Control::MountOk { mountpoint } => Ok(mountpoint),
            Control::Error { code, message } => Err(BrokerError { code, message }.into()),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// Unmount every snapshot mount of this session (end-of-cycle).
    pub fn unmount_all(&mut self) -> Result<(), ClientError> {
        self.send(&Control::UnmountAll)?;
        self.expect_ok()
    }

    /// Ask the broker to unmount everything and exit.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.send(&Control::Shutdown)?;
        self.expect_ok()
    }

    fn expect_ok(&mut self) -> Result<(), ClientError> {
        match self.recv()? {
            Control::Ok => Ok(()),
            Control::Error { code, message } => Err(BrokerError { code, message }.into()),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    fn send(&mut self, msg: &Control) -> Result<(), ClientError> {
        write_control(&mut self.stream, msg)?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Control, ClientError> {
        Ok(read_control(&mut self.stream)?)
    }
}
