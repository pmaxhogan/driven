//! The root mount broker's server loop (DESIGN s5.3.2).
//!
//! Runs as root, serves the tiny [`crate::protocol::Control`] vocabulary over
//! a unix-domain socket, and holds the ONLY privileged operations in the
//! macOS snapshot design: `mount_apfs -s` (mount a named APFS local snapshot
//! read-only), `umount`, and `tmutil deletelocalsnapshots`.
//!
//! # Trust boundary (mirrors DESIGN s5.3.1)
//!
//! The connecting app is UNTRUSTED. Defences, in order:
//! - The socket lives in the app user's `0700` runtime dir at an unguessable
//!   path; after binding, the broker `chown`s it to the app user with mode
//!   `0600`, so only that user (and root) can connect at all.
//! - Every accepted connection is authenticated: `getpeereid` must return the
//!   `--peer-uid` fixed at launch, and the peer pid's executable
//!   (`proc_pidpath` via `LOCAL_PEERPID`) must live in the broker's own
//!   directory - so a different same-user process cannot drive the broker.
//!   (Pid-reuse TOCTOU is the same documented residual as the Windows helper;
//!   signature verification is the same documented hardening follow-up.)
//! - Every request is re-validated from scratch: snapshot names/dates against
//!   the strict `tmutil` shapes ([`crate::snapshot`]), volumes against the
//!   `--allowed-volume` list fixed at launch. Mountpoints are broker-chosen
//!   under a root-owned directory - never client-supplied.
//! - Mounts are always `rdonly,nosuid,nodev` and NEVER `noowners`
//!   (post-CVE-2020-9771 that flag is a fingerprinted TCC-bypass signature
//!   and useless anyway; ownership-preserving mounts are exactly what lets
//!   the un-elevated app read only what it could already read).
//!
//! The pure decision pieces (mount registry reuse, allow-list matching,
//! request validation) are unit-tested cross-OS; the socket/mount syscalls
//! are macOS-gated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::protocol::Control;
use crate::snapshot::{is_valid_snapshot_date, is_valid_snapshot_name};

/// Prefix for the broker's root-owned mountpoint directories, swept for stale
/// leftovers at startup (nothing in the platform cleans up after a crash).
pub const MOUNT_ROOT_PREFIX: &str = "driven-apfs-mounts-";

/// The parent under which per-session mount roots are created.
pub const MOUNT_ROOT_PARENT: &str = "/private/var/run";

/// A validated mount request, produced only by [`validate_mount_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRequest {
    /// The allow-listed volume mount point.
    pub volume_mount: PathBuf,
    /// The validated snapshot name.
    pub snapshot_name: String,
}

/// Boundary validation for a [`Control::MountSnapshot`] request. The volume
/// must EXACTLY match an allow-listed entry (no prefix/containment games -
/// the allow-list is short and app-fixed, so exact string paths are the
/// least-surprise contract) and the snapshot name must be a strict
/// `com.apple.TimeMachine.<date>.local`.
pub fn validate_mount_request(
    volume_mount: &str,
    snapshot_name: &str,
    allowed_volumes: &[PathBuf],
) -> Result<MountRequest, (&'static str, String)> {
    if !is_valid_snapshot_name(snapshot_name) {
        return Err((
            "invalid_request",
            "snapshot name does not match the required shape".to_string(),
        ));
    }
    let vol = Path::new(volume_mount);
    if !vol.is_absolute() {
        return Err((
            "invalid_request",
            "volume mount must be absolute".to_string(),
        ));
    }
    if !allowed_volumes.iter().any(|a| a.as_path() == vol) {
        return Err((
            "not_allowed",
            "volume not in the launch allow-list".to_string(),
        ));
    }
    Ok(MountRequest {
        volume_mount: vol.to_path_buf(),
        snapshot_name: snapshot_name.to_string(),
    })
}

/// Boundary validation for a [`Control::DeleteSnapshot`] request.
pub fn validate_delete_request(snapshot_date: &str) -> Result<(), (&'static str, String)> {
    if !is_valid_snapshot_date(snapshot_date) {
        return Err((
            "invalid_request",
            "snapshot date does not match the required shape".to_string(),
        ));
    }
    Ok(())
}

/// The broker's in-memory registry of live mounts: one mount per
/// (volume, snapshot) pair, reused across requests within the session -
/// mirroring the Windows one-snapshot-per-volume-per-cycle contract.
#[derive(Debug, Default)]
pub struct MountRegistry {
    /// (volume mount, snapshot name) -> mountpoint.
    mounts: HashMap<(PathBuf, String), PathBuf>,
    /// Monotonic index for mountpoint names (`m0`, `m1`, ...).
    next_index: u64,
}

impl MountRegistry {
    /// Existing mountpoint for this (volume, snapshot), if already mounted.
    pub fn existing(&self, volume: &Path, snapshot: &str) -> Option<&PathBuf> {
        self.mounts
            .get(&(volume.to_path_buf(), snapshot.to_string()))
    }

    /// Reserve the next mountpoint path under `mount_root` and record it.
    pub fn record(&mut self, volume: &Path, snapshot: &str, mount_root: &Path) -> PathBuf {
        let mp = mount_root.join(format!("m{}", self.next_index));
        self.next_index += 1;
        self.mounts
            .insert((volume.to_path_buf(), snapshot.to_string()), mp.clone());
        mp
    }

    /// Drain every recorded mountpoint (for unmount-all / shutdown).
    pub fn drain(&mut self) -> Vec<PathBuf> {
        let mps = self.mounts.drain().map(|(_, mp)| mp).collect();
        self.next_index = 0;
        mps
    }

    /// Number of live mounts.
    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{self};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::launch::HelperArgs;
    use crate::protocol::{read_control, write_control, PROTOCOL_VERSION};

    /// Everything one session's server shares across connections.
    struct ServerShared {
        allowed_volumes: Vec<PathBuf>,
        peer_uid: u32,
        helper_dir: PathBuf,
        mount_root: PathBuf,
        registry: Mutex<MountRegistry>,
    }

    /// Run the broker. Refuses to run as non-root. Returns only on fatal
    /// setup errors; a clean `Shutdown` exits the process.
    pub fn run(args: HelperArgs) -> io::Result<()> {
        // SAFETY: geteuid is always safe to call.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "driven-apfs-helper must run as root",
            ));
        }

        sweep_stale_mount_roots();

        // Root-owned per-session mount root: 0755 so the app user can
        // traverse INTO the (ownership-preserving) mounted snapshots.
        let mount_root = Path::new(MOUNT_ROOT_PARENT).join(format!(
            "{}{}",
            MOUNT_ROOT_PREFIX,
            std::process::id()
        ));
        std::fs::create_dir_all(&mount_root)?;

        // Bind the socket, then hand it to the app user (0600): only that
        // user and root can connect.
        if args.socket.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "socket path already exists; refusing to replace it",
            ));
        }
        validate_socket_dir(&args.socket, args.peer_uid)?;
        let listener = UnixListener::bind(&args.socket)?;
        chown_chmod(&args.socket, args.peer_uid, 0o600)?;

        let helper_dir = std::env::current_exe()?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| io::Error::other("helper has no parent directory"))?;

        let shared = Arc::new(ServerShared {
            allowed_volumes: args.allowed_volumes.clone(),
            peer_uid: args.peer_uid,
            helper_dir,
            mount_root,
            registry: Mutex::new(MountRegistry::default()),
        });

        spawn_app_pid_watcher(args.app_pid, Arc::clone(&shared), args.socket.clone());

        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                if let Err(e) = serve_connection(stream, &shared) {
                    tracing::debug!(error = %e, "apfs broker connection ended with error");
                }
            });
        }
        Ok(())
    }

    /// The socket's parent dir must be owned by the peer uid with mode 0700 -
    /// the app created it; a world-writable or foreign-owned dir means the
    /// unguessable-path premise is void, so refuse to serve there.
    fn validate_socket_dir(socket: &Path, peer_uid: u32) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let dir = socket
            .parent()
            .ok_or_else(|| io::Error::other("socket path has no parent"))?;
        let md = std::fs::metadata(dir)?;
        if md.uid() != peer_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "socket directory is not owned by the expected user",
            ));
        }
        if md.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "socket directory must be mode 0700",
            ));
        }
        Ok(())
    }

    fn chown_chmod(path: &Path, uid: u32, mode: u32) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        // SAFETY: valid NUL-terminated path; chown/chmod are simple syscalls.
        if unsafe { libc::chown(c.as_ptr(), uid, u32::MAX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Authenticate a connection: `getpeereid` uid must match, and the peer
    /// pid's executable must live in the broker's own directory.
    fn authenticate_peer(stream: &UnixStream, shared: &ServerShared) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        let fd = stream.as_raw_fd();

        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (u32::MAX, u32::MAX);
        // SAFETY: fd is a valid connected unix socket; out-params are valid.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err("getpeereid failed".to_string());
        }
        if uid != shared.peer_uid {
            return Err(format!("peer uid {uid} is not the expected user"));
        }

        // LOCAL_PEERPID -> proc_pidpath: the peer executable's directory must
        // be the broker's own directory (app + helper ship side by side).
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: valid fd, valid out-params sized for pid_t.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                &mut pid as *mut _ as *mut libc::c_void,
                &mut len,
            )
        } != 0
        {
            return Err("LOCAL_PEERPID failed".to_string());
        }
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: buf is writable for its whole length.
        let n = unsafe {
            libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32)
        };
        if n <= 0 {
            return Err("proc_pidpath failed".to_string());
        }
        buf.truncate(n as usize);
        let exe = PathBuf::from(String::from_utf8_lossy(&buf).into_owned());
        match exe.parent() {
            Some(dir) if dir == shared.helper_dir => Ok(()),
            _ => Err("peer executable is not co-installed with the helper".to_string()),
        }
    }

    fn serve_connection(stream: UnixStream, shared: &ServerShared) -> io::Result<()> {
        if let Err(why) = authenticate_peer(&stream, shared) {
            tracing::warn!(%why, "apfs broker rejected a connection");
            // Do not answer unauthenticated peers at all.
            return Ok(());
        }
        let mut reader = stream.try_clone()?;
        let mut writer = stream;

        loop {
            let msg = match read_control(&mut reader) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            match msg {
                Control::Hello { protocol_version } => {
                    if protocol_version != PROTOCOL_VERSION {
                        write_control(
                            &mut writer,
                            &Control::Error {
                                code: "protocol_mismatch".into(),
                                message: format!(
                                    "broker speaks v{PROTOCOL_VERSION}, client sent v{protocol_version}"
                                ),
                            },
                        )?;
                        return Ok(());
                    }
                    write_control(
                        &mut writer,
                        &Control::HelloOk {
                            protocol_version: PROTOCOL_VERSION,
                        },
                    )?;
                }
                Control::MountSnapshot {
                    volume_mount,
                    snapshot_name,
                } => {
                    let reply = handle_mount(&volume_mount, &snapshot_name, shared);
                    write_control(&mut writer, &reply)?;
                }
                Control::UnmountAll => {
                    unmount_all(shared);
                    write_control(&mut writer, &Control::Ok)?;
                }
                Control::DeleteSnapshot { snapshot_date } => {
                    let reply = handle_delete(&snapshot_date);
                    write_control(&mut writer, &reply)?;
                }
                Control::Shutdown => {
                    unmount_all(shared);
                    let _ = std::fs::remove_dir(&shared.mount_root);
                    write_control(&mut writer, &Control::Ok)?;
                    std::process::exit(0);
                }
                // Server-to-client vocabulary arriving at the server is a
                // protocol violation.
                Control::HelloOk { .. }
                | Control::MountOk { .. }
                | Control::Ok
                | Control::Error { .. } => {
                    write_control(
                        &mut writer,
                        &Control::Error {
                            code: "invalid_request".into(),
                            message: "unexpected message direction".into(),
                        },
                    )?;
                }
            }
        }
    }

    fn handle_mount(volume_mount: &str, snapshot_name: &str, shared: &ServerShared) -> Control {
        let req = match validate_mount_request(volume_mount, snapshot_name, &shared.allowed_volumes)
        {
            Ok(r) => r,
            Err((code, message)) => {
                return Control::Error {
                    code: code.into(),
                    message,
                }
            }
        };
        let mut reg = shared.registry.lock().expect("mount registry lock");
        if let Some(mp) = reg.existing(&req.volume_mount, &req.snapshot_name) {
            return Control::MountOk {
                mountpoint: mp.to_string_lossy().into_owned(),
            };
        }
        let mp = reg.record(&req.volume_mount, &req.snapshot_name, &shared.mount_root);
        drop(reg);

        if let Err(e) = std::fs::create_dir_all(&mp) {
            return Control::Error {
                code: "io_error".into(),
                message: format!("create mountpoint: {e}"),
            };
        }
        // Ownership-preserving read-only mount; NEVER `noowners` (see module
        // docs). nosuid/nodev harden the mounted tree.
        let out = Command::new("/sbin/mount_apfs")
            .arg("-o")
            .arg("rdonly,nosuid,nodev")
            .arg("-s")
            .arg(&req.snapshot_name)
            .arg(&req.volume_mount)
            .arg(&mp)
            .output();
        match out {
            Ok(o) if o.status.success() => Control::MountOk {
                mountpoint: mp.to_string_lossy().into_owned(),
            },
            Ok(o) => {
                let _ = std::fs::remove_dir(&mp);
                Control::Error {
                    code: "mount_failed".into(),
                    message: format!(
                        "mount_apfs exited {}: {}",
                        o.status,
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                }
            }
            Err(e) => {
                let _ = std::fs::remove_dir(&mp);
                Control::Error {
                    code: "io_error".into(),
                    message: format!("mount_apfs spawn: {e}"),
                }
            }
        }
    }

    fn handle_delete(snapshot_date: &str) -> Control {
        if let Err((code, message)) = validate_delete_request(snapshot_date) {
            return Control::Error {
                code: code.into(),
                message,
            };
        }
        let out = Command::new("/usr/bin/tmutil")
            .arg("deletelocalsnapshots")
            .arg(snapshot_date)
            .output();
        match out {
            // A snapshot already thinned away deletes as a failure exit -
            // treat any "delete" outcome as idempotent success; the backstop
            // (APFS auto-thinning) makes strictness pointless.
            Ok(_) => Control::Ok,
            Err(e) => Control::Error {
                code: "io_error".into(),
                message: format!("tmutil spawn: {e}"),
            },
        }
    }

    fn unmount_all(shared: &ServerShared) {
        let mps = shared.registry.lock().expect("mount registry lock").drain();
        for mp in mps {
            unmount_one(&mp);
        }
    }

    fn unmount_one(mp: &Path) {
        let ok = Command::new("/sbin/umount")
            .arg(mp)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            // Force-unmount as the fallback; a busy mount left behind is
            // swept at next broker startup.
            let _ = Command::new("/sbin/umount").arg("-f").arg(mp).output();
        }
        let _ = std::fs::remove_dir(mp);
    }

    /// Sweep mount roots stranded by a crashed prior session: unmount every
    /// child mountpoint and remove the directories.
    fn sweep_stale_mount_roots() {
        let Ok(entries) = std::fs::read_dir(MOUNT_ROOT_PARENT) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(MOUNT_ROOT_PREFIX) {
                continue;
            }
            let root = entry.path();
            if let Ok(children) = std::fs::read_dir(&root) {
                for child in children.flatten() {
                    unmount_one(&child.path());
                }
            }
            let _ = std::fs::remove_dir(&root);
        }
    }

    /// Exit (after unmounting) when the app process goes away, so a crashed
    /// app never leaves a root broker behind. Polling `kill(pid, 0)` at a
    /// coarse interval is the portable idiom.
    fn spawn_app_pid_watcher(app_pid: u32, shared: Arc<ServerShared>, socket: PathBuf) {
        std::thread::spawn(move || loop {
            // SAFETY: kill with signal 0 only checks existence.
            let alive = unsafe { libc::kill(app_pid as libc::pid_t, 0) } == 0;
            if !alive {
                unmount_all(&shared);
                let _ = std::fs::remove_dir(&shared.mount_root);
                let _ = std::fs::remove_file(&socket);
                std::process::exit(0);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
    }
}

#[cfg(target_os = "macos")]
pub use macos::run;

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<PathBuf> {
        vec![PathBuf::from("/System/Volumes/Data"), PathBuf::from("/")]
    }

    #[test]
    fn mount_request_accepts_allowed_volume_and_valid_name() {
        let r = validate_mount_request(
            "/System/Volumes/Data",
            "com.apple.TimeMachine.2026-07-29-154532.local",
            &allowed(),
        )
        .unwrap();
        assert_eq!(r.volume_mount, PathBuf::from("/System/Volumes/Data"));
    }

    #[test]
    fn mount_request_rejects_unlisted_volume() {
        let err = validate_mount_request(
            "/Volumes/Other",
            "com.apple.TimeMachine.2026-07-29-154532.local",
            &allowed(),
        )
        .unwrap_err();
        assert_eq!(err.0, "not_allowed");
    }

    #[test]
    fn mount_request_rejects_bad_names_and_relative_volumes() {
        assert_eq!(
            validate_mount_request("/", "not-a-snapshot", &allowed())
                .unwrap_err()
                .0,
            "invalid_request"
        );
        assert_eq!(
            validate_mount_request(
                "relative",
                "com.apple.TimeMachine.2026-07-29-154532.local",
                &allowed()
            )
            .unwrap_err()
            .0,
            "invalid_request"
        );
    }

    #[test]
    fn mount_request_requires_exact_volume_match_no_prefix_games() {
        // A path UNDER an allowed volume is still not an allowed volume.
        let err = validate_mount_request(
            "/System/Volumes/Data/Users",
            "com.apple.TimeMachine.2026-07-29-154532.local",
            &allowed(),
        )
        .unwrap_err();
        assert_eq!(err.0, "not_allowed");
    }

    #[test]
    fn delete_request_validates_the_date() {
        assert!(validate_delete_request("2026-07-29-154532").is_ok());
        assert_eq!(
            validate_delete_request("2026-07-29").unwrap_err().0,
            "invalid_request"
        );
        assert_eq!(
            validate_delete_request("2026-07-29-154532 --all")
                .unwrap_err()
                .0,
            "invalid_request"
        );
    }

    #[test]
    fn registry_reuses_mounts_per_volume_snapshot_pair() {
        let mut reg = MountRegistry::default();
        let root = Path::new("/private/var/run/driven-apfs-mounts-1");
        let vol = Path::new("/System/Volumes/Data");
        let snap = "com.apple.TimeMachine.2026-07-29-154532.local";

        assert!(reg.existing(vol, snap).is_none());
        let mp = reg.record(vol, snap, root);
        assert_eq!(mp, root.join("m0"));
        assert_eq!(reg.existing(vol, snap), Some(&mp));

        // A different snapshot on the same volume gets its own mountpoint.
        let mp2 = reg.record(vol, "com.apple.TimeMachine.2026-07-29-160000.local", root);
        assert_eq!(mp2, root.join("m1"));
        assert_eq!(reg.len(), 2);

        let mut drained = reg.drain();
        drained.sort();
        assert_eq!(drained, vec![root.join("m0"), root.join("m1")]);
        assert!(reg.is_empty());
    }
}
