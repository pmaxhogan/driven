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
    /// SKIPPED rather than run against an unsuitable host. Phase-2 fills in
    /// the per-OS probes (elevation token, NTFS volume enumeration, VSS,
    /// `LongPathsEnabled` registry read); the interface fixes the shape and
    /// the env-driven bits that are platform-agnostic.
    pub fn probe() -> Self {
        let real_drive_creds = std::env::var("DRIVEN_E2E_REFRESH_TOKEN").is_ok()
            && std::env::var("DRIVEN_E2E_DEST_FOLDER_ID").is_ok();
        Self {
            admin: false,
            ntfs_volume: None,
            case_sensitive_volume: None,
            free_disk_bytes: 0,
            real_drive_creds,
            vss_available: false,
            long_paths_enabled: false,
            network_reachable: false,
        }
    }
}
