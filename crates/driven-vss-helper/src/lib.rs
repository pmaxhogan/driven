//! `driven-vss-helper` - the least-privilege VSS elevation helper (DESIGN
//! s5.3.1, issue #25).
//!
//! VSS snapshot creation needs Administrator rights. Rather than elevate the
//! whole backup app (its OAuth tokens, its network stack, its Drive
//! credentials), Driven elevates ONLY the shadow-copy operation: a small
//! privileged broker (`driven-vss-helper.exe`) creates the snapshot and streams
//! the locked file's bytes back to the un-elevated app over a secured Windows
//! named pipe.
//!
//! # Surface split
//!
//! OS-independent (compiles + unit-tests on every target):
//! - [`protocol`] - the length-prefixed, capped wire framing + the tiny control
//!   vocabulary.
//! - [`validate`] - the boundary input validation (volume + allowed-roots) the
//!   helper runs on every request; the un-elevated caller is untrusted.
//! - [`auth`] - the pipe security-descriptor SDDL and the process-image
//!   identity DECISION logic (the SID lookup + pipe/process syscalls are
//!   Windows-gated inside it).
//! - [`launch`] - the elevated-launch argv + pipe-name construction (the
//!   `ShellExecute runas` call itself is Windows-gated).
//! - [`BrokeredVssProvider`] - the app-side [`driven_vss::VssProvider`] that
//!   reads locked files through the helper, fitting the existing executor seam.
//!
//! Windows-only (the real named pipe + VSS + streaming):
//! - [`run_server`] - the elevated helper's accept/authenticate/serve loop.
//! - [`HelperClient`] - the un-elevated client the provider drives.

pub mod auth;
pub mod launch;
pub mod protocol;
pub mod validate;

mod provider;
pub use launch::HelperLauncher;
pub use provider::BrokeredVssProvider;

#[cfg(windows)]
mod client;
#[cfg(windows)]
mod server;

#[cfg(windows)]
pub use client::HelperClient;
#[cfg(windows)]
pub use server::run_server;
