//! The un-elevated client side of the helper pipe (DESIGN s5.3.1).
//!
//! Windows only. It opens `\\.\pipe\driven-vss-<...>`, verifies the SERVER end
//! is the expected `driven-vss-helper.exe` in the app's install directory
//! (before sending any path), completes the version handshake, and then drives
//! the tiny request/response protocol: open a locked file, pull its bytes in
//! chunks, close it, end the cycle, or shut the helper down.

#![cfg(windows)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;

use crate::auth::is_sibling_image;
use crate::auth::windows_impl::server_image_path;
use crate::protocol::{
    read_control, read_data, write_control, Control, MAX_DATA_FRAME, PROTOCOL_VERSION,
};

/// Overall budget for establishing the connection: the server may still be
/// creating its next pipe instance, so a short bounded retry beats a spurious
/// failure. Locked-file backup is not latency-critical.
const CONNECT_BUDGET: Duration = Duration::from_secs(5);

/// A connected, authenticated client session to the helper.
pub struct HelperClient {
    io: File,
}

impl HelperClient {
    /// Connect to `pipe_name`, verify the server image is the helper in
    /// `expected_helper_dir`, and complete the handshake.
    pub fn connect(pipe_name: &str, expected_helper_dir: &Path) -> Result<Self, String> {
        let io = open_pipe_with_retry(pipe_name)?;

        // Verify the SERVER end before sending anything sensitive.
        let raw = HANDLE(io.as_raw_handle());
        let server_img = server_image_path(raw)?;
        let reference = expected_helper_dir.join("driven-vss-helper.exe");
        if !is_sibling_image(&reference, &server_img) {
            return Err(format!(
                "server identity check failed: {} is not the expected helper",
                server_img.display()
            ));
        }

        let mut client = Self { io };
        client.send(&Control::Hello {
            protocol_version: PROTOCOL_VERSION,
        })?;
        match client.recv()? {
            Control::HelloOk { protocol_version } if protocol_version == PROTOCOL_VERSION => {
                Ok(client)
            }
            Control::HelloOk { protocol_version } => Err(format!(
                "helper protocol version mismatch: got {protocol_version}, want {PROTOCOL_VERSION}"
            )),
            other => Err(format!("unexpected handshake reply: {other:?}")),
        }
    }

    /// Ask the helper to snapshot `volume` and open the locked file at
    /// `live_path`. Returns the file size on success.
    pub fn open_locked(&mut self, volume: &str, live_path: &str) -> Result<u64, String> {
        self.send(&Control::OpenLocked {
            volume: volume.to_string(),
            live_path: live_path.to_string(),
        })?;
        match self.recv()? {
            Control::OpenOk { size } => Ok(size),
            Control::Error { code, message } => Err(format!("{code}: {message}")),
            other => Err(format!("unexpected open reply: {other:?}")),
        }
    }

    /// Pull the next chunk of the open file into `buf`. Returns the number of
    /// bytes written (0 = EOF).
    pub fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        let want = buf.len().min(MAX_DATA_FRAME);
        self.send(&Control::Read {
            max_len: want as u32,
        })?;
        let data = read_data(&mut self.io).map_err(|e| format!("read data frame: {e}"))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// Close the currently-open file (its snapshot stays cached for the cycle).
    pub fn close_file(&mut self) -> Result<(), String> {
        self.send(&Control::CloseFile)?;
        self.expect_ok()
    }

    /// Release every snapshot the helper created this cycle.
    pub fn end_cycle(&mut self) -> Result<(), String> {
        self.send(&Control::EndCycle)?;
        self.expect_ok()
    }

    /// Release everything and shut the helper process down.
    pub fn shutdown(&mut self) -> Result<(), String> {
        self.send(&Control::Shutdown)?;
        self.expect_ok()
    }

    fn expect_ok(&mut self) -> Result<(), String> {
        match self.recv()? {
            Control::Ok => Ok(()),
            Control::Error { code, message } => Err(format!("{code}: {message}")),
            other => Err(format!("unexpected reply: {other:?}")),
        }
    }

    fn send(&mut self, msg: &Control) -> Result<(), String> {
        write_control(&mut self.io, msg).map_err(|e| format!("write control: {e}"))?;
        self.io.flush().map_err(|e| format!("flush: {e}"))
    }

    fn recv(&mut self) -> Result<Control, String> {
        read_control(&mut self.io).map_err(|e| format!("read control: {e}"))
    }
}

/// Open the client end of the named pipe, retrying briefly while the server is
/// still creating its next instance (ERROR_FILE_NOT_FOUND) or busy
/// (ERROR_PIPE_BUSY).
fn open_pipe_with_retry(pipe_name: &str) -> Result<File, String> {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;

    let deadline = Instant::now() + CONNECT_BUDGET;
    loop {
        match OpenOptions::new().read(true).write(true).open(pipe_name) {
            Ok(f) => return Ok(f),
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0);
                if (code == ERROR_FILE_NOT_FOUND || code == ERROR_PIPE_BUSY)
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                return Err(format!("open pipe {pipe_name}: {e}"));
            }
        }
    }
}

/// Read `Read`/write helpers used by the provider: implement `std::io::Read`
/// over the client so a locked file can be copied with `std::io::copy`.
impl Read for HelperClient {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_chunk(buf).map_err(std::io::Error::other)
    }
}
