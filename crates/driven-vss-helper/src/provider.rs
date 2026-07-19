//! [`BrokeredVssProvider`] - the app-side [`driven_vss::VssProvider`] that reads
//! locked files through the elevated helper instead of in-process (DESIGN
//! s5.3.1).
//!
//! It fits the EXISTING executor seam: `map_for_volume` streams the locked
//! file's bytes from the helper into a short-lived temp file the un-elevated app
//! owns and returns THAT path, so the executor's `read_path`
//! open/identity/encrypt/upload pipeline is unchanged. Temp copies are deleted
//! at `end_cycle`. Off Windows - or when the helper is not reachable - every
//! `map_for_volume` returns [`SnapshotOutcome::Unavailable`], so the executor
//! degrades to skip-the-locked-file exactly as it does with no elevation today.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use driven_vss::{SnapshotOutcome, VssMode, VssProvider};

/// The app-side provider that brokers VSS reads through the elevated helper.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct BrokeredVssProvider {
    mode: Mutex<VssMode>,
    /// The named pipe the helper serves on (shared with the launched helper).
    pipe_name: String,
    /// The app's install directory - the helper must be a sibling here, and the
    /// client verifies the server's image lives here.
    helper_dir: PathBuf,
    /// Where streamed temp copies of locked files land (app-owned).
    temp_dir: PathBuf,
    /// Whether the helper is currently believed reachable. Set by [`Self::probe`]
    /// and cleared on a connection failure.
    available: AtomicBool,
    /// Serialises helper access: locked files are rare, so one connection at a
    /// time keeps the server single-instance and simple.
    guard: Mutex<()>,
    /// Temp copies created this cycle, deleted at [`VssProvider::end_cycle`].
    temp_files: Mutex<Vec<PathBuf>>,
}

impl BrokeredVssProvider {
    /// Build a brokered provider. `available` starts `false`; call [`Self::probe`]
    /// after the helper is launched to confirm reachability. Off Windows the
    /// provider is permanently unavailable.
    pub fn new(
        mode: VssMode,
        pipe_name: impl Into<String>,
        helper_dir: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mode: Mutex::new(mode),
            pipe_name: pipe_name.into(),
            helper_dir: helper_dir.into(),
            temp_dir: temp_dir.into(),
            available: AtomicBool::new(false),
            guard: Mutex::new(()),
            temp_files: Mutex::new(Vec::new()),
        }
    }

    fn current_mode(&self) -> VssMode {
        match self.mode.lock() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        }
    }

    /// The pipe name the helper serves on (so the launcher passes the SAME one).
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Try to reach the helper (connect + handshake, then disconnect) and update
    /// [`Self::available`]. Returns the new availability. Off Windows: always
    /// `false`.
    pub fn probe(&self) -> bool {
        #[cfg(windows)]
        {
            let _g = self.guard.lock();
            match crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir) {
                Ok(_client) => {
                    self.available.store(true, Ordering::SeqCst);
                    true
                }
                Err(e) => {
                    tracing::debug!(error = %e, "VSS helper: probe failed; unavailable");
                    self.available.store(false, Ordering::SeqCst);
                    false
                }
            }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Ask the helper to shut down (release everything + exit). Best-effort;
    /// called at app shutdown. Off Windows: no-op.
    pub fn shutdown_helper(&self) {
        #[cfg(windows)]
        {
            let _g = self.guard.lock();
            if let Ok(mut c) =
                crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir)
            {
                let _ = c.shutdown();
            }
        }
    }

    #[cfg(windows)]
    fn map_via_helper(&self, live_path: &std::path::Path) -> SnapshotOutcome {
        use std::io::Write;

        let Some(volume) = crate::validate::drive_of(live_path) else {
            tracing::warn!(path = %live_path.display(), "VSS helper: no drive letter; degrading");
            return SnapshotOutcome::Unavailable;
        };

        let _g = self.guard.lock();
        let mut client =
            match crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "VSS helper: connect failed; degrading to skip");
                    self.available.store(false, Ordering::SeqCst);
                    return SnapshotOutcome::Unavailable;
                }
            };

        let size = match client.open_locked(&volume, &live_path.to_string_lossy()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %live_path.display(), "VSS helper: open failed; degrading to skip");
                return SnapshotOutcome::Unavailable;
            }
        };

        // Stream the bytes into an app-owned temp file the executor can read.
        if let Err(e) = std::fs::create_dir_all(&self.temp_dir) {
            tracing::warn!(error = %e, "VSS helper: temp dir create failed; degrading");
            return SnapshotOutcome::Unavailable;
        }
        let temp = self
            .temp_dir
            .join(format!("driven-vss-{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut out = match std::fs::File::create(&temp) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "VSS helper: temp file create failed; degrading");
                return SnapshotOutcome::Unavailable;
            }
        };
        let copied = match std::io::copy(&mut client, &mut out) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, path = %live_path.display(), "VSS helper: stream failed; degrading");
                let _ = std::fs::remove_file(&temp);
                return SnapshotOutcome::Unavailable;
            }
        };
        let _ = out.flush();
        let _ = client.close_file();
        drop(client);

        if copied != size {
            tracing::warn!(
                copied,
                expected = size,
                path = %live_path.display(),
                "VSS helper: streamed byte count did not match reported size; degrading"
            );
            let _ = std::fs::remove_file(&temp);
            return SnapshotOutcome::Unavailable;
        }

        self.temp_files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(temp.clone());
        SnapshotOutcome::Mapped(temp)
    }
}

impl VssProvider for BrokeredVssProvider {
    fn map_for_volume(&self, live_path: &std::path::Path) -> SnapshotOutcome {
        if self.current_mode() == VssMode::Never || !self.available.load(Ordering::SeqCst) {
            return SnapshotOutcome::Unavailable;
        }
        #[cfg(windows)]
        {
            self.map_via_helper(live_path)
        }
        #[cfg(not(windows))]
        {
            let _ = live_path;
            SnapshotOutcome::Unavailable
        }
    }

    fn mode(&self) -> VssMode {
        self.current_mode()
    }

    fn set_mode(&self, mode: VssMode) {
        match self.mode.lock() {
            Ok(mut g) => *g = mode,
            Err(p) => *p.into_inner() = mode,
        }
    }

    fn available(&self) -> bool {
        cfg!(windows)
            && self.current_mode() != VssMode::Never
            && self.available.load(Ordering::SeqCst)
    }

    fn end_cycle(&self) {
        // Delete this cycle's temp copies.
        let files: Vec<PathBuf> = {
            let mut guard = self.temp_files.lock().unwrap_or_else(|p| p.into_inner());
            guard.drain(..).collect()
        };
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        // Tell the helper to release its per-cycle snapshots (best-effort).
        #[cfg(windows)]
        {
            if self.available.load(Ordering::SeqCst) {
                let _g = self.guard.lock();
                if let Ok(mut c) =
                    crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir)
                {
                    let _ = c.end_cycle();
                }
            }
        }
    }

    // recorded_snapshots(): the helper owns the real shadow copies under a
    // VSS_CTX_BACKUP (auto-release) context bounded by the helper process
    // lifetime, so the app keeps no cross-process orphan ledger - the default
    // empty impl is correct here.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_mode_and_unprobed_are_unavailable_and_degrade() {
        let p = BrokeredVssProvider::new(
            VssMode::Auto,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        // Not probed => unavailable => degrade regardless of OS.
        assert!(!p.available());
        assert_eq!(
            p.map_for_volume(std::path::Path::new(r"C:\Users\me\Documents\f.pst")),
            SnapshotOutcome::Unavailable
        );

        let n = BrokeredVssProvider::new(
            VssMode::Never,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        assert!(!n.available(), "never mode is never available");
    }

    #[test]
    fn set_mode_never_disables() {
        let p = BrokeredVssProvider::new(
            VssMode::Auto,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        p.set_mode(VssMode::Never);
        assert_eq!(p.mode(), VssMode::Never);
        assert!(!p.available());
    }

    #[test]
    fn end_cycle_is_safe_with_no_temp_files() {
        let p = BrokeredVssProvider::new(
            VssMode::Auto,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        p.end_cycle(); // must not panic
    }

    #[test]
    fn end_cycle_deletes_recorded_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", dir.path());
        // Simulate a streamed temp copy.
        let temp = dir.path().join("driven-vss-fake.tmp");
        std::fs::write(&temp, b"stale").unwrap();
        p.temp_files.lock().unwrap().push(temp.clone());
        assert!(temp.exists());
        p.end_cycle();
        assert!(!temp.exists(), "end_cycle must delete temp copies");
    }
}
