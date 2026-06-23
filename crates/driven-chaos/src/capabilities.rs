//! Host capability probe (STRESS_HARNESS s2.5).
//!
//! [`CapabilitySet`] records what the host can offer the harness. It is
//! probed once at startup and cached for the whole run. A scenario whose
//! [`crate::scenario::Scenario::requires`] set is not satisfied is
//! reported SKIPPED with the exact list of missing items - capability
//! gaps never turn a run red (STRESS_HARNESS s1.1).
//!
//! The probe is deliberately conservative: when a check cannot be made
//! cheaply or safely it reports the capability ABSENT, so a scenario is
//! skipped rather than run against an environment that cannot honour it.

use std::path::PathBuf;

/// What the host can offer, probed once and cached for the run
/// (STRESS_HARNESS s2.5).
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    /// Elevated on Windows; euid 0 / `CAP_SYS_ADMIN` on Linux.
    pub admin: bool,
    /// A drive letter that is NTFS (Windows only).
    pub ntfs_volume: Option<char>,
    /// Mountpoint of an ext4 / APFS-cs / NTFS-cs-flagged path.
    pub case_sensitive_volume: Option<PathBuf>,
    /// Free bytes on the volume backing the harness temp dir.
    pub free_disk_bytes: u64,
    /// `DRIVEN_E2E_REFRESH_TOKEN` present plus a throwaway folder id.
    pub real_drive_creds: bool,
    /// Windows + admin (VSS needs both).
    pub vss_available: bool,
    /// Windows registry `HKLM\...\LongPathsEnabled == 1`.
    pub long_paths_enabled: bool,
    /// The host has real Internet (for the few scenarios that need it).
    pub network_reachable: bool,
    /// `DRIVEN_CHAOS_SOAK=1` is set, opting this host into the soak-grade
    /// massive-input rows.
    pub soak: bool,
    /// `DRIVEN_CHAOS_ALLOW_DISK_MOUNT=1` is set, opting this host into the
    /// constrained-volume disk-full row.
    pub disk_mount_allowed: bool,
}

/// A single capability a scenario can require. The dotted/`cap:` rendering
/// matches the privilege column in the STRESS_HARNESS s3 catalogue so the
/// report lines line up with the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// Elevated process (Windows admin / Linux root).
    Admin,
    /// Any NTFS volume is present.
    NtfsVolume,
    /// A case-sensitive volume is present.
    CaseSensitiveVolume,
    /// At least `min` free bytes on the harness volume.
    FreeDiskBytes {
        /// Minimum free bytes the scenario needs to materialise its fixture.
        min: u64,
    },
    /// Real Drive E2E credentials are configured.
    RealDriveCreds,
    /// VSS is usable (Windows + admin).
    VssAvailable,
    /// Long paths are enabled (Windows).
    LongPathsEnabled,
    /// Real Internet is reachable.
    NetworkReachable,
    /// The host OS family must be Windows.
    Windows,
    /// The host OS family must be Unix (Linux or macOS).
    Unix,
    /// Soak mode is opted into via `DRIVEN_CHAOS_SOAK=1`. The massive-input
    /// rows (`million-files-nested`, `tiny-files-100k-in-one-dir`) require it so
    /// they RUN in the weekly soak job (which sets the env) but SKIP cleanly in
    /// the per-PR hermetic matrix - their multi-minute 100k-1M-file scan is
    /// soak-grade (STRESS_HARNESS s3.2), not PR-gating work. Recorded as a
    /// missing capability, never faked or weakened.
    Soak,
    /// Mounting a throwaway constrained volume + a Driven write-into-source path
    /// is opted into via `DRIVEN_CHAOS_ALLOW_DISK_MOUNT=1`. The `disk-full-target`
    /// row requires it so it SKIPs cleanly everywhere today (the env is never
    /// set): the core ENOSPC->local.disk_full mapping is implemented + unit-
    /// tested, but V1's read-only source path cannot induce it end to end, so
    /// the row stays a recorded documented gap rather than a FAIL on an elevated
    /// CI runner (where the bare Admin gate would otherwise let it run + bail).
    DiskMountAllowed,
}

impl Capability {
    /// Human/`cap:`-style label used in SKIPPED report rows.
    pub fn label(&self) -> String {
        match self {
            Capability::Admin => "admin".to_string(),
            Capability::NtfsVolume => "cap:ntfs_volume".to_string(),
            Capability::CaseSensitiveVolume => "cap:case_sensitive_volume".to_string(),
            Capability::FreeDiskBytes { min } => format!("cap:free_disk_bytes>={min}"),
            Capability::RealDriveCreds => "cap:real_drive_creds".to_string(),
            Capability::VssAvailable => "cap:vss_available".to_string(),
            Capability::LongPathsEnabled => "cap:long_paths_enabled".to_string(),
            Capability::NetworkReachable => "cap:network_reachable".to_string(),
            Capability::Windows => "platform:windows".to_string(),
            Capability::Unix => "platform:unix".to_string(),
            Capability::Soak => "cap:soak".to_string(),
            Capability::DiskMountAllowed => "cap:disk_mount_allowed".to_string(),
        }
    }

    /// Whether `set` satisfies this single capability.
    pub fn is_satisfied_by(&self, set: &CapabilitySet) -> bool {
        match self {
            Capability::Admin => set.admin,
            Capability::NtfsVolume => set.ntfs_volume.is_some(),
            Capability::CaseSensitiveVolume => set.case_sensitive_volume.is_some(),
            Capability::FreeDiskBytes { min } => set.free_disk_bytes >= *min,
            Capability::RealDriveCreds => set.real_drive_creds,
            Capability::VssAvailable => set.vss_available,
            Capability::LongPathsEnabled => set.long_paths_enabled,
            Capability::NetworkReachable => set.network_reachable,
            Capability::Windows => cfg!(windows),
            Capability::Unix => cfg!(unix),
            Capability::Soak => set.soak,
            Capability::DiskMountAllowed => set.disk_mount_allowed,
        }
    }
}

/// The set of capabilities a scenario requires from the host. An empty set
/// means the scenario runs everywhere.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRequirements {
    /// Required capabilities; every one must hold for the scenario to run.
    pub required: Vec<Capability>,
}

impl CapabilityRequirements {
    /// An empty requirement set (runs on any host).
    pub fn none() -> Self {
        Self::default()
    }

    /// Build a requirement set from a list of capabilities.
    pub fn of(required: Vec<Capability>) -> Self {
        Self { required }
    }

    /// Return the labels of every required capability the host does NOT
    /// satisfy. An empty result means the scenario can run.
    pub fn missing(&self, set: &CapabilitySet) -> Vec<String> {
        self.required
            .iter()
            .filter(|c| !c.is_satisfied_by(set))
            .map(Capability::label)
            .collect()
    }
}

impl CapabilitySet {
    /// Probe the host once for every capability and cache the result.
    ///
    /// Conservative by construction: a check that cannot be performed
    /// safely reports the capability absent so the dependent scenario is
    /// SKIPPED rather than run against an unsuitable host (a capability gap
    /// never turns a run red, STRESS_HARNESS s1.1).
    pub fn probe() -> Self {
        let real_drive_creds = std::env::var("DRIVEN_E2E_REFRESH_TOKEN").is_ok()
            && std::env::var("DRIVEN_E2E_DEST_FOLDER_ID").is_ok();
        // The volume backing the harness temp dir is where every fixture is
        // materialised, so free-disk + filesystem-type probes target it.
        let temp_dir = std::env::temp_dir();
        let admin = driven_vss::is_elevated();
        let free_disk_bytes = probe_free_disk_bytes(&temp_dir);
        let ntfs_volume = probe_ntfs_volume(&temp_dir);
        let case_sensitive_volume = probe_case_sensitive_volume(&temp_dir);
        let long_paths_enabled = probe_long_paths_enabled();
        // VSS needs Windows + elevation (driven-vss only exposes the COM
        // sequence on an elevated Windows host).
        let vss_available = cfg!(windows) && admin;
        let network_reachable = probe_network_reachable();
        let soak = std::env::var("DRIVEN_CHAOS_SOAK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let disk_mount_allowed = std::env::var("DRIVEN_CHAOS_ALLOW_DISK_MOUNT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            admin,
            ntfs_volume,
            case_sensitive_volume,
            free_disk_bytes,
            real_drive_creds,
            vss_available,
            long_paths_enabled,
            network_reachable,
            soak,
            disk_mount_allowed,
        }
    }
}

/// Free bytes available to the current user on the volume backing `path`.
/// Returns 0 (the conservative "no space" reading) on any probe failure.
#[cfg(windows)]
fn probe_free_disk_bytes(path: &std::path::Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_to_caller: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path; the out-param is a valid
    // local. We pass null for the other two out-params (allowed by the API).
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_to_caller as *mut u64),
            None,
            None,
        )
    };
    if ok.is_ok() {
        free_to_caller
    } else {
        0
    }
}

/// Free bytes available on the filesystem backing `path` via `statvfs`.
/// Returns 0 on any probe failure.
#[cfg(unix)]
fn probe_free_disk_bytes(path: &std::path::Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let mut cpath: Vec<u8> = path.as_os_str().as_bytes().to_vec();
    cpath.push(0);
    // SAFETY: `cpath` is a NUL-terminated C string; `stat` is zeroed and valid
    // for the duration of the call. We read fields only on a 0 return.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr() as *const libc::c_char, &mut stat) == 0 {
            (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64)
        } else {
            0
        }
    }
}

/// Neither Windows nor Unix: no portable free-disk probe.
#[cfg(not(any(windows, unix)))]
fn probe_free_disk_bytes(_path: &std::path::Path) -> u64 {
    0
}

/// The drive letter of `path`'s volume if that volume's filesystem is NTFS
/// (Windows only). `None` off Windows or when the filesystem cannot be read.
#[cfg(windows)]
fn probe_ntfs_volume(path: &std::path::Path) -> Option<char> {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

    // The volume root, e.g. `C:\`. Derive it from the path's first component.
    let s = path.to_string_lossy();
    let drive = s.chars().next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    let root: Vec<u16> = format!("{drive}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut fs_name = [0u16; 32];
    // SAFETY: `root` is a NUL-terminated UTF-16 volume root; `fs_name` is a
    // local buffer the API fills with the filesystem name. All other out-params
    // are null (permitted).
    let ok = unsafe {
        GetVolumeInformationW(
            windows::core::PCWSTR(root.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    };
    if ok.is_err() {
        return None;
    }
    let len = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let name = String::from_utf16_lossy(&fs_name[..len]);
    if name.eq_ignore_ascii_case("NTFS") {
        Some(drive.to_ascii_uppercase())
    } else {
        None
    }
}

/// Off Windows there is no NTFS volume concept.
#[cfg(not(windows))]
fn probe_ntfs_volume(_path: &std::path::Path) -> Option<char> {
    None
}

/// A case-sensitive mountpoint, detected empirically by creating two paths
/// that differ only in case under a temp dir and checking they are distinct.
/// Returns the probed directory on success, `None` otherwise.
fn probe_case_sensitive_volume(temp_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let probe_dir = temp_dir.join(format!("driven-chaos-case-probe-{}", std::process::id()));
    if std::fs::create_dir_all(&probe_dir).is_err() {
        return None;
    }
    let lower = probe_dir.join("casetest");
    let upper = probe_dir.join("CASETEST");
    let result = (|| {
        std::fs::write(&lower, b"l").ok()?;
        // If writing the upper-case name reads back the lower-case content, the
        // filesystem folded the case (case-insensitive). A case-sensitive FS
        // keeps them distinct.
        std::fs::write(&upper, b"upper").ok()?;
        let lower_bytes = std::fs::read(&lower).ok()?;
        if lower_bytes == b"l" {
            // `lower` still holds its own content -> the two names are distinct
            // -> the volume is case-sensitive.
            Some(temp_dir.to_path_buf())
        } else {
            None
        }
    })();
    let _ = std::fs::remove_dir_all(&probe_dir);
    result
}

/// Whether Windows long-path support is enabled
/// (`HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled == 1`).
/// Probed empirically by attempting to create a directory whose path exceeds
/// the legacy `MAX_PATH` (260) limit under the temp dir: success implies long
/// paths are usable. `false` off Windows (the legacy limit is Windows-only).
#[cfg(windows)]
fn probe_long_paths_enabled() -> bool {
    let base = std::env::temp_dir().join(format!("driven-chaos-lp-{}", std::process::id()));
    // Build a path comfortably over MAX_PATH using nested 50-char segments,
    // WITHOUT the \\?\ prefix (which would bypass the very limit we test).
    let mut deep = base.clone();
    for _ in 0..6 {
        deep.push("x".repeat(50));
    }
    let enabled = std::fs::create_dir_all(&deep).is_ok();
    let _ = std::fs::remove_dir_all(&base);
    enabled
}

/// Off Windows the legacy MAX_PATH limit does not apply, so the capability is
/// not meaningful; report it absent (no Windows-only scenario should run).
#[cfg(not(windows))]
fn probe_long_paths_enabled() -> bool {
    false
}

/// Whether the host has real outbound Internet, probed by a short-timeout TCP
/// connect to a well-known resolver (no DNS dependency: dial the IP directly).
/// `false` on any failure so a network scenario SKIPs rather than flaking.
fn probe_network_reachable() -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    // 8.8.8.8:53 (Google DNS) - reachable from any host with real Internet,
    // dialled by IP so a broken local resolver does not mask connectivity.
    let addr: SocketAddr = ([8, 8, 8, 8], 53).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok()
}
