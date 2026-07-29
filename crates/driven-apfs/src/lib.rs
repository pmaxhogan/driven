//! macOS locked-file fallback via APFS local snapshots (DESIGN s5.3.2).
//!
//! The macOS sibling of the Windows least-privilege VSS helper (DESIGN
//! s5.3.1), plugging into the SAME [`driven_vss::VssProvider`] seam the
//! executor's open path already consults - the pure
//! [`driven_vss::fallback_decision`] contract is untouched.
//!
//! # Why the shape differs from Windows
//!
//! - **Snapshot creation is unprivileged.** `tmutil localsnapshot` proxies via
//!   XPC to Apple's `backupd`, which holds the private
//!   `com.apple.private.vfs.snapshot` entitlement. Driven cannot call
//!   `fs_snapshot_create(2)` itself at ANY privilege level (the public
//!   `com.apple.developer.vfs.snapshot` entitlement is DTS-request-gated and
//!   unavailable to an unsigned tool), so `tmutil` is the design, not a
//!   workaround.
//! - **Only the mount needs root.** `mount_apfs -s` requires root, so that -
//!   and the matching unmount/delete - is ALL the root broker does.
//! - **No byte streaming.** Snapshot mounts preserve ownership (Driven never
//!   passes `-o noowners`: post-CVE-2020-9771 it is both useless as a TCC
//!   bypass and an EDR-fingerprinted signature), so the un-elevated app reads
//!   the user's own files directly from the mounted snapshot. The Windows
//!   helper must stream bytes because `\\?\GLOBALROOT` devices are unreadable
//!   un-elevated; the macOS broker's privileged surface is mount/unmount only.
//! - **Snapshots do NOT bypass TCC.** A TCC-protected path (Mail, Messages...)
//!   is still EPERM through a snapshot mount without Full Disk Access. The
//!   snapshot fallback therefore serves BUSY/locked files only; TCC denials
//!   are handled by FDA onboarding, not by this crate.
//!
//! # Trust boundary (mirrors DESIGN s5.3.1)
//!
//! The un-elevated app is UNTRUSTED by the broker. Everything crossing the
//! socket (a volume mount point + a snapshot name) is re-validated from
//! scratch against the allow-list fixed on the broker's command line at
//! launch. The broker authenticates its peer (uid + executable identity); the
//! app authenticates the broker (socket created root-owned at an app-chosen
//! unguessable path).

#[cfg(target_os = "macos")]
pub mod client;
pub mod launch;
pub mod paths;
pub mod protocol;
pub mod provider;
pub mod server;
pub mod snapshot;

pub use launch::{HelperLaunchStatus, HelperLauncher, LaunchError, OsascriptLauncher};
pub use provider::ApfsBrokeredProvider;
