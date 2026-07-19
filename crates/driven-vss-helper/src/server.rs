//! The elevated named-pipe server the helper binary runs (DESIGN s5.3.1).
//!
//! Windows only. It must run elevated; it creates ONE pipe instance at a time
//! (locked-file backup is the rare case, so the client serialises requests),
//! authenticates each connecting client's image against the helper's own
//! install directory, validates every `OpenLocked` request against the
//! configured backup roots (lexically AND after canonicalising the directory
//! chain), and streams the locked file's bytes from a per-volume VSS snapshot it
//! creates + reuses for the cycle. The un-elevated client never touches the
//! `\\?\GLOBALROOT` shadow device - only the helper does.

#![cfg(windows)]

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};

use driven_vss::VssSnapshot;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, GENERIC_READ, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::auth::windows_impl::{client_image_path, current_user_sid_string, SecurityDescriptor};
use crate::auth::{is_sibling_image, pipe_sddl};
use crate::protocol::{read_control, write_control, write_data, Control, MAX_DATA_FRAME};
use crate::validate::{check_within_roots, validate_open_request};

/// Share-mode flags so opening the (read-only) snapshot file never trips on a
/// concurrent handle: FILE_SHARE_READ | WRITE | DELETE.
const FILE_SHARE_ALL: u32 = 0x0000_0007;

/// Pipe buffer sizes (bytes). A data chunk is up to [`MAX_DATA_FRAME`]; a modest
/// kernel buffer keeps the stream flowing without hogging non-paged pool.
const PIPE_BUF: u32 = 128 * 1024;

/// Run the helper server loop until a client asks it to `Shutdown` (or a fatal
/// setup error). Serves one connection at a time; the per-volume snapshot cache
/// persists across connections for the cycle and is released on `EndCycle` /
/// `Shutdown` (and, as a backstop, on process exit via each snapshot's RAII
/// `Drop`).
pub fn run_server(pipe_name: &str, allowed_roots: Vec<PathBuf>) -> Result<(), String> {
    if !driven_vss::is_elevated() {
        return Err("the VSS helper must run elevated (Administrator)".to_string());
    }
    let helper_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    // Canonicalise the configured roots ONCE for the defence-in-depth check; a
    // root that cannot be canonicalised (e.g. removed) is simply not usable.
    let roots_canonical: Vec<PathBuf> = allowed_roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();

    let user_sid = current_user_sid_string()?;
    let sddl = pipe_sddl(&user_sid);
    let pipe_wide: Vec<u16> = std::ffi::OsStr::new(pipe_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // The per-cycle snapshot cache: volume label ("C:") -> live snapshot.
    let mut cache: HashMap<String, VssSnapshot> = HashMap::new();

    tracing::info!(
        pipe = pipe_name,
        roots = allowed_roots.len(),
        "VSS helper: server listening"
    );

    loop {
        // A fresh security descriptor per instance (owned + freed each loop).
        let sd = SecurityDescriptor::from_sddl(&sddl)?;
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_ptr(),
            bInheritHandle: false.into(),
        };

        // SAFETY: valid null-terminated pipe name; `sa` points at a live SD that
        // outlives the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1, // one instance at a time (serialised client)
                PIPE_BUF,
                PIPE_BUF,
                0,
                Some(&sa as *const SECURITY_ATTRIBUTES),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: reading the thread's last-error immediately after the fail.
            let code = unsafe { GetLastError() };
            return Err(format!("CreateNamedPipe failed (error {})", code.0));
        }
        // `sa` is no longer needed once the instance exists.
        let _ = &mut sa;

        // Wait for a client. ERROR_PIPE_CONNECTED means one connected between
        // create and connect - also success.
        // SAFETY: `handle` is a live server pipe instance.
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        if let Err(e) = connected {
            if e.code() != windows::core::HRESULT::from(ERROR_PIPE_CONNECTED) {
                tracing::warn!(error = %e, "VSS helper: ConnectNamedPipe failed");
                // SAFETY: closing the instance we created.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                continue;
            }
        }

        // Authenticate the client image BEFORE any I/O.
        let authorised = match client_image_path(handle) {
            Ok(img) => is_sibling_image(&helper_exe, &img),
            Err(e) => {
                tracing::warn!(error = %e, "VSS helper: could not resolve client image; rejecting");
                false
            }
        };
        if !authorised {
            tracing::warn!("VSS helper: rejecting unauthorised client connection");
            // SAFETY: live handle; disconnect + close the rejected connection.
            unsafe {
                let _ = DisconnectNamedPipe(handle);
                let _ = CloseHandle(handle);
            }
            continue;
        }

        // Wrap the handle in a File for std::io framing. `File` OWNS the handle
        // now and closes it on drop, tearing down this instance.
        // SAFETY: `handle` is a valid, connected, authorised pipe handle we hand
        // to File exclusively (not used raw afterwards).
        let mut io = unsafe { File::from_raw_handle(handle.0) };

        let shutdown = handle_connection(&mut io, &mut cache, &roots_canonical, &allowed_roots);
        drop(io); // closes the handle / instance

        if shutdown {
            tracing::info!("VSS helper: shutdown requested; releasing snapshots and exiting");
            break;
        }
    }

    // Explicitly release everything (RAII Drop would also do this at process
    // exit; being explicit keeps diff-area space free promptly).
    cache.clear();
    Ok(())
}

/// Serve one connected client until it disconnects or asks to `Shutdown`.
/// Returns `true` iff the client asked the server to shut the process down.
fn handle_connection(
    io: &mut File,
    cache: &mut HashMap<String, VssSnapshot>,
    roots_canonical: &[PathBuf],
    roots_lexical: &[PathBuf],
) -> bool {
    // The single locked file this connection is currently streaming.
    let mut open_file: Option<File> = None;

    loop {
        let msg = match read_control(io) {
            Ok(m) => m,
            // Any read error (including a clean client disconnect) ends this
            // connection; the server loops to accept the next one.
            Err(_) => return false,
        };

        match msg {
            Control::Hello {
                protocol_version: _,
            } => {
                let _ = write_control(
                    io,
                    &Control::HelloOk {
                        protocol_version: crate::protocol::PROTOCOL_VERSION,
                    },
                );
            }
            Control::OpenLocked { volume, live_path } => {
                match do_open(cache, roots_canonical, roots_lexical, &volume, &live_path) {
                    Ok((file, size)) => {
                        open_file = Some(file);
                        let _ = write_control(io, &Control::OpenOk { size });
                    }
                    Err((code, message)) => {
                        open_file = None;
                        let _ = write_control(io, &Control::Error { code, message });
                    }
                }
            }
            Control::Read { max_len } => {
                // The client only sends Read after a successful OpenOk, so a
                // missing open file is a protocol violation - surface it.
                let Some(file) = open_file.as_mut() else {
                    let _ = write_control(
                        io,
                        &Control::Error {
                            code: "invalid_request".into(),
                            message: "no file is open".into(),
                        },
                    );
                    continue;
                };
                let want = (max_len as usize).min(MAX_DATA_FRAME);
                let mut buf = vec![0u8; want];
                match file.read(&mut buf) {
                    Ok(n) => {
                        buf.truncate(n);
                        // n == 0 => empty data frame => EOF.
                        if write_data(io, &buf).is_err() {
                            return false;
                        }
                    }
                    Err(_) => {
                        // A read error mid-stream: send an empty frame so the
                        // client stops, then drop the file (`take` both releases
                        // it and counts as a use for the unused-assignment lint).
                        let _ = write_data(io, &[]);
                        let _ = open_file.take();
                    }
                }
            }
            Control::CloseFile => {
                open_file = None;
                let _ = write_control(io, &Control::Ok);
            }
            Control::EndCycle => {
                open_file = None;
                let released = cache.len();
                cache.clear();
                if released > 0 {
                    tracing::info!(released, "VSS helper: released per-cycle snapshots");
                }
                let _ = write_control(io, &Control::Ok);
            }
            Control::Shutdown => {
                // `open_file` drops with the function return; no need to null it.
                cache.clear();
                let _ = write_control(io, &Control::Ok);
                return true;
            }
            // Server -> client messages must never arrive from the client.
            Control::HelloOk { .. }
            | Control::OpenOk { .. }
            | Control::Ok
            | Control::Error { .. } => {
                let _ = write_control(
                    io,
                    &Control::Error {
                        code: "invalid_request".into(),
                        message: "unexpected message".into(),
                    },
                );
            }
        }
    }
}

/// Validate an `OpenLocked` request, ensure the volume's snapshot exists
/// (creating + caching it), map the live path under the shadow copy, and open
/// it for streaming. Returns `(open_file, size)` or a `(code, message)` error.
fn do_open(
    cache: &mut HashMap<String, VssSnapshot>,
    roots_canonical: &[PathBuf],
    roots_lexical: &[PathBuf],
    volume: &str,
    live_path: &str,
) -> Result<(File, u64), (String, String)> {
    let live = PathBuf::from(live_path);

    // 1. Lexical validation against the as-configured roots.
    let norm_volume = validate_open_request(roots_lexical, volume, &live)
        .map_err(|e| (e.code().to_string(), e.to_string()))?;

    // 2. Defence in depth: canonicalise the directory chain (resolving any
    //    symlinked directory) and re-check against the canonical roots.
    let canonical = canonicalize_leaf(&live).map_err(|m| ("invalid_request".to_string(), m))?;
    if !check_within_roots(roots_canonical, &canonical) {
        return Err((
            "not_allowed".to_string(),
            "resolved path is outside the configured backup roots".to_string(),
        ));
    }

    // 3. Ensure a snapshot for the volume (lazy create, cache for the cycle).
    if !cache.contains_key(&norm_volume) {
        let snap = VssSnapshot::create(&norm_volume)
            .map_err(|e| ("vss_unavailable".to_string(), format!("snapshot: {e}")))?;
        tracing::info!(volume = %norm_volume, "VSS helper: created per-cycle snapshot");
        cache.insert(norm_volume.clone(), snap);
    }
    let snap = cache
        .get(&norm_volume)
        .expect("snapshot just inserted for this volume");

    // 4. Map the live path under the shadow device and open it for reading.
    let mapped = snap
        .map_path(&live)
        .map_err(|e| ("io_error".to_string(), format!("map path: {e}")))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ.0)
        .share_mode(FILE_SHARE_ALL)
        .open(&mapped)
        .map_err(|e| ("io_error".to_string(), format!("open snapshot file: {e}")))?;
    let size = file
        .metadata()
        .map_err(|e| ("io_error".to_string(), format!("stat: {e}")))?
        .len();
    Ok((file, size))
}

/// Canonicalise the PARENT directory of `live` (resolving symlinked
/// directories) and re-attach the file name, WITHOUT opening the (locked) leaf.
fn canonicalize_leaf(live: &Path) -> Result<PathBuf, String> {
    let parent = live
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;
    let file_name = live
        .file_name()
        .ok_or_else(|| "path has no file name".to_string())?;
    let canon_parent =
        std::fs::canonicalize(parent).map_err(|e| format!("canonicalise parent: {e}"))?;
    Ok(canon_parent.join(file_name))
}
