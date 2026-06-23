//! [`VssProvider`] - the per-cycle snapshot seam the executor's open path
//! consults (ROADMAP M3.5, DESIGN s5.3).
//!
//! # Lifecycle: the CYCLE owns the snapshots, not a single source or file
//!
//! A snapshot is created lazily on first need and REUSED for the rest of the
//! cycle. The cycle - not `execute()` - owns release: an orchestrator cycle
//! runs every enabled source, and two sources can live on the same volume
//! (`C:\Users\a`, `C:\Users\b`), so releasing per-source/per-file would create
//! N snapshots per volume per cycle and break the "ONE per volume, reuse,
//! release at cycle end" contract. The orchestrator holds the provider and
//! calls [`VssProvider::end_cycle`] after its source loop on EVERY exit path
//! (including a mid-cycle error), which clears the cache and releases each
//! snapshot via its RAII [`Drop`](crate::VssSnapshot).
//!
//! # The map-for-volume operation
//!
//! [`VssProvider::map_for_volume`] is the one call the executor makes when a
//! live open returned [`OpenAttempt::Locked`](crate::OpenAttempt) (auto) or
//! unconditionally (always): "give me the snapshot-mapped path for this live
//! file, creating + caching the volume's snapshot if needed". It returns a
//! [`SnapshotOutcome`] the pure [`fallback_decision`](crate::fallback_decision)
//! then turns into an open instruction.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::{RecordedSnapshot, SnapshotOutcome, VssMode};

/// A synchronous recorder the orchestrator injects so a freshly-created shadow
/// copy's GUID is durably noted the instant it exists (P1-A), before
/// [`VssProvider::map_for_volume`] returns the mapped path. Called with the
/// snapshot GUID string and its creation time (Unix epoch ms). It runs inline
/// on the provider's (blocking-friendly) map path, so it must be cheap and must
/// not block on async I/O - the orchestrator's wiring pushes into an in-memory
/// process-wide ledger it later flushes to the durable registry under its lock.
pub type SnapshotRecorder = Arc<dyn Fn(&str, i64) + Send + Sync>;

/// The per-cycle snapshot provider the executor's open path consults.
///
/// Object-safe so the executor can hold `Arc<dyn VssProvider>` and tests
/// inject [`FakeVssProvider`]. All methods are sync: snapshot creation is a
/// blocking COM sequence the caller already runs on a blocking-friendly path
/// (the executor's per-op task), and the cross-OS fake is trivially sync.
pub trait VssProvider: Send + Sync {
    /// Ensure a snapshot exists for the volume hosting `live_path` (creating
    /// and caching it on first need for this cycle), then return the
    /// snapshot-mapped path for `live_path`.
    ///
    /// Returns [`SnapshotOutcome::Unavailable`] when VSS cannot help - not
    /// elevated, `vss_mode = never`, off Windows, or any COM failure - so the
    /// caller degrades to skip-the-locked-file. Never panics; a COM error is
    /// logged and folded into `Unavailable`.
    fn map_for_volume(&self, live_path: &Path) -> SnapshotOutcome;

    /// The configured mode (so the executor can decide whether to consult VSS
    /// at all before calling [`Self::map_for_volume`]).
    fn mode(&self) -> VssMode;

    /// Apply a (possibly changed) mode for the next cycle. The orchestrator
    /// calls this from `reconfigure` so a `vss_mode` setting change actually
    /// takes effect instead of being frozen at provider construction. The
    /// default is a no-op for providers whose mode is immutable.
    fn set_mode(&self, mode: VssMode) {
        let _ = mode;
    }

    /// Whether VSS is fundamentally available this run (elevated + Windows).
    /// Cheap; the executor reads it to short-circuit the open path.
    fn available(&self) -> bool;

    /// Install the record-at-create hook (P1-A): a synchronous callback the
    /// provider invokes the instant it creates a shadow copy (GUID + creation
    /// ms), before returning the mapped path, so a crash during the ensuing
    /// locked-file upload still leaves the shadow recorded. The orchestrator
    /// wires this to push into its process-wide in-memory ledger. Default is a
    /// no-op for providers that do not create real shadows.
    fn set_recorder(&self, recorder: SnapshotRecorder) {
        let _ = recorder;
    }

    /// Release every snapshot created this cycle (RAII drop). Called by the
    /// orchestrator after the per-source loop, on all exit paths. Idempotent.
    fn end_cycle(&self);

    /// The shadow copies CURRENTLY held by this provider (GUID + creation
    /// time), for the orphan-cleanup ledger. The orchestrator reads this AFTER
    /// the source loop (while the snapshots still exist) and persists the
    /// entries, so a later run can release any that an unclean shutdown
    /// stranded (RAII [`crate::VssSnapshot::drop`] never ran). Empty by default
    /// (fakes, off-Windows) - then the registry stays empty and cleanup is a
    /// no-op.
    fn recorded_snapshots(&self) -> Vec<RecordedSnapshot> {
        Vec::new()
    }
}

// -----------------------------------------------------------------------------
// RealVssProvider
// -----------------------------------------------------------------------------

/// The production provider: lazily snapshots each volume once per cycle and
/// maps locked-file paths under the shadow-copy device root.
///
/// Holds the snapshot cache behind a [`Mutex`] keyed by the uppercased volume
/// (`C:`). Off Windows - or un-elevated, or `vss_mode = never` - it reports
/// unavailable and every `map_for_volume` returns `Unavailable`, so the
/// executor degrades exactly as the no-VSS path does today.
pub struct RealVssProvider {
    /// Interior-mutable so `reconfigure` can apply a changed `vss_mode` between
    /// cycles (P1-5); read under the lock on every `mode`/`available` check.
    mode: Mutex<VssMode>,
    elevated: bool,
    /// Per-cycle snapshot cache, keyed by uppercased volume label (`C:`).
    /// `Mutex` (not `RwLock`) because the check-then-create must be atomic so
    /// two concurrent locked files on one volume create exactly one snapshot.
    inner: Mutex<ProviderInner>,
    /// Optional record-at-create hook (P1-A). When set, it is called
    /// SYNCHRONOUSLY the moment a shadow copy is created - before the mapped
    /// path is returned - so a crash during the ensuing (possibly long) locked
    /// file upload still leaves the GUID recorded. `None` => no hook (the
    /// historical behaviour: the orchestrator records via `recorded_snapshots`
    /// after each source). Held behind a `Mutex` only to keep the struct
    /// `Sync` with an interior-settable closure; read briefly under the lock.
    recorder: Mutex<Option<SnapshotRecorder>>,
}

#[derive(Default)]
struct ProviderInner {
    /// Volume label (`C:`) -> the live snapshot for that volume this cycle
    /// plus the wall-clock ms it was created (for the orphan ledger).
    /// `cfg(windows)` only holds real handles; off Windows it is always empty
    /// because `available()` is false and `map_for_volume` short-circuits.
    #[cfg(windows)]
    snapshots: std::collections::HashMap<String, (crate::VssSnapshot, i64)>,
}

/// Current wall-clock time in Unix epoch milliseconds (for the orphan ledger).
/// A pre-1970 clock (impossible in practice) reads as 0.
#[cfg(windows)]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl RealVssProvider {
    /// Build a provider for `mode`. Detects elevation once up front
    /// ([`crate::is_elevated`]); when not elevated (or `mode = never`, or off
    /// Windows) the provider is permanently unavailable for this run.
    pub fn new(mode: VssMode) -> Self {
        let elevated = crate::is_elevated();
        Self {
            mode: Mutex::new(mode),
            elevated,
            inner: Mutex::new(ProviderInner::default()),
            recorder: Mutex::new(None),
        }
    }

    /// Invoke the record-at-create hook, if one is installed. Cheap; runs under
    /// no other lock (the caller releases `inner` is NOT required - this only
    /// touches `recorder`).
    #[cfg(windows)]
    fn record_at_create(&self, guid: &str, created_ms: i64) {
        let hook = match self.recorder.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        if let Some(hook) = hook {
            hook(guid, created_ms);
        }
    }

    /// Read the current mode under the lock (recovering from poisoning).
    fn current_mode(&self) -> VssMode {
        match self.mode.lock() {
            Ok(g) => *g,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    /// `true` when VSS could actually run: a usable mode, elevated, on Windows.
    fn is_available(&self) -> bool {
        cfg!(windows) && self.elevated && self.current_mode() != VssMode::Never
    }
}

impl VssProvider for RealVssProvider {
    fn mode(&self) -> VssMode {
        self.current_mode()
    }

    fn set_mode(&self, mode: VssMode) {
        match self.mode.lock() {
            Ok(mut g) => *g = mode,
            Err(poisoned) => *poisoned.into_inner() = mode,
        }
    }

    fn available(&self) -> bool {
        self.is_available()
    }

    fn set_recorder(&self, recorder: SnapshotRecorder) {
        match self.recorder.lock() {
            Ok(mut g) => *g = Some(recorder),
            Err(poisoned) => *poisoned.into_inner() = Some(recorder),
        }
    }

    #[cfg(windows)]
    fn map_for_volume(&self, live_path: &Path) -> SnapshotOutcome {
        if !self.is_available() {
            return SnapshotOutcome::Unavailable;
        }
        let Some(volume) = volume_label(live_path) else {
            tracing::warn!(path = %live_path.display(), "VSS: cannot derive volume; degrading");
            return SnapshotOutcome::Unavailable;
        };

        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Lazily create + cache one snapshot per volume per cycle.
        if !inner.snapshots.contains_key(&volume) {
            match crate::VssSnapshot::create(&volume) {
                Ok(snap) => {
                    // P1-A: record the GUID SYNCHRONOUSLY at create time, before
                    // returning the mapped path, so a crash during the ensuing
                    // (possibly long) locked-file upload still leaves the shadow
                    // recorded for the next run's orphan sweep. Capture the
                    // identity now while we still own the handle.
                    let created = now_ms();
                    let guid = snap.snapshot_id_string();
                    tracing::info!(volume = %volume, "VSS: created per-cycle snapshot");
                    inner.snapshots.insert(volume.clone(), (snap, created));
                    // Drop the `inner` lock before running the hook so the
                    // recorder (which may take its own locks) never nests under
                    // the provider's snapshot mutex.
                    drop(inner);
                    self.record_at_create(&guid, created);
                    // Re-acquire to map the path below.
                    inner = match self.inner.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
                Err(err) => {
                    tracing::warn!(volume = %volume, %err, "VSS: snapshot creation failed; degrading to skip");
                    return SnapshotOutcome::Unavailable;
                }
            }
        }

        // Checked lookup (not `expect`): the recorder hook above ran with the
        // `inner` lock briefly dropped, so a concurrent `end_cycle` could have
        // cleared the cache in that gap. The trait is `Send + Sync` and exposes
        // both methods, so degrade rather than panic in non-test code.
        let Some((snap, _created)) = inner.snapshots.get(&volume) else {
            tracing::warn!(volume = %volume, "VSS: snapshot vanished before map (concurrent end_cycle); degrading to skip");
            return SnapshotOutcome::Unavailable;
        };
        match snap.map_path(live_path) {
            Ok(mapped) => SnapshotOutcome::Mapped(mapped),
            Err(err) => {
                tracing::warn!(path = %live_path.display(), %err, "VSS: path map failed; degrading to skip");
                SnapshotOutcome::Unavailable
            }
        }
    }

    #[cfg(not(windows))]
    fn map_for_volume(&self, _live_path: &Path) -> SnapshotOutcome {
        // No VSS off Windows; always degrade.
        SnapshotOutcome::Unavailable
    }

    fn end_cycle(&self) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        #[cfg(windows)]
        {
            // Dropping each VssSnapshot releases its shadow copy (RAII).
            let n = inner.snapshots.len();
            inner.snapshots.clear();
            if n > 0 {
                tracing::info!(
                    released = n,
                    "VSS: released per-cycle snapshots at cycle end"
                );
            }
        }
        // Off Windows the cache is always empty; nothing to do.
        let _ = &mut inner;
    }

    #[cfg(windows)]
    fn recorded_snapshots(&self) -> Vec<RecordedSnapshot> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner
            .snapshots
            .values()
            .map(|(snap, created)| RecordedSnapshot {
                snapshot_id: snap.snapshot_id_string(),
                created_at_ms: *created,
            })
            .collect()
    }

    #[cfg(not(windows))]
    fn recorded_snapshots(&self) -> Vec<RecordedSnapshot> {
        Vec::new()
    }
}

/// Extract the uppercased volume label (`C:`) from an absolute Windows path.
/// Returns `None` for a path without a drive prefix (UNC, relative).
#[cfg(windows)]
fn volume_label(path: &Path) -> Option<String> {
    use std::path::{Component, Prefix};
    let mut comps = path.components();
    match comps.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                // `letter` is the ASCII drive letter byte; normalise to `C:`.
                Some(format!("{}:", (letter as char).to_ascii_uppercase()))
            }
            // UNC / device prefixes have no single VSS volume letter.
            _ => None,
        },
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// FakeVssProvider (tests)
// -----------------------------------------------------------------------------

/// A deterministic [`VssProvider`] for cross-OS tests: returns a configured
/// [`SnapshotOutcome`] without touching COM, and counts `end_cycle` calls so a
/// test can assert the orchestrator released snapshots.
///
/// The default ([`FakeVssProvider::unavailable`]) reports VSS unavailable so
/// the degrade-gracefully contract (locked file -> skipped + reported) is the
/// path under test on every OS, including CI.
pub struct FakeVssProvider {
    /// Interior-mutable so a test can exercise the P1-5 `set_mode` seam and
    /// assert that switching to [`VssMode::Never`] makes the provider report
    /// unavailable.
    mode: Mutex<VssMode>,
    available: std::sync::atomic::AtomicBool,
    /// What `map_for_volume` returns. `None` => `Unavailable`; `Some(root)` =>
    /// `Mapped(root.join(<file name>))` so a test can assert a plausible
    /// mapped path.
    mapped_root: Option<std::path::PathBuf>,
    /// Snapshots this fake reports via [`VssProvider::recorded_snapshots`], so
    /// an orchestrator test can exercise the orphan-registry persistence path
    /// without real COM.
    recorded: Vec<RecordedSnapshot>,
    end_cycle_calls: std::sync::atomic::AtomicUsize,
    map_calls: std::sync::atomic::AtomicUsize,
    /// How many times `recorded_snapshots` was called, so a test can assert the
    /// orchestrator records orphans PER SOURCE (P1-2), not only once after the
    /// loop.
    recorded_calls: std::sync::atomic::AtomicUsize,
    /// Optional record-at-create spy (P1-A). When set, a successful
    /// `map_for_volume` (one that returns `Mapped`, simulating a created
    /// shadow) invokes it with a synthetic GUID + creation time, so an
    /// orchestrator test can assert the GUID is recorded SYNCHRONOUSLY at
    /// create time without real COM.
    recorder: Mutex<Option<SnapshotRecorder>>,
    /// A deterministic GUID the fake hands the recorder on create.
    recorder_guid: String,
}

impl FakeVssProvider {
    /// A provider that always reports VSS unavailable (the degrade path).
    /// Mode defaults to [`VssMode::Auto`].
    pub fn unavailable() -> Self {
        Self {
            mode: Mutex::new(VssMode::Auto),
            available: std::sync::atomic::AtomicBool::new(false),
            mapped_root: None,
            recorded: Vec::new(),
            end_cycle_calls: std::sync::atomic::AtomicUsize::new(0),
            map_calls: std::sync::atomic::AtomicUsize::new(0),
            recorded_calls: std::sync::atomic::AtomicUsize::new(0),
            recorder: Mutex::new(None),
            recorder_guid: "{fake-snapshot-guid}".to_string(),
        }
    }

    /// A provider that reports available and maps every file under
    /// `mapped_root` (simulating a successful snapshot). Lets a test exercise
    /// the `OpenSnapshot` branch without real COM.
    pub fn mapped_under(mode: VssMode, mapped_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            mode: Mutex::new(mode),
            available: std::sync::atomic::AtomicBool::new(true),
            mapped_root: Some(mapped_root.into()),
            recorded: Vec::new(),
            end_cycle_calls: std::sync::atomic::AtomicUsize::new(0),
            map_calls: std::sync::atomic::AtomicUsize::new(0),
            recorded_calls: std::sync::atomic::AtomicUsize::new(0),
            recorder: Mutex::new(None),
            recorder_guid: "{fake-snapshot-guid}".to_string(),
        }
    }

    /// Set the snapshots this fake reports via
    /// [`VssProvider::recorded_snapshots`] (for the orphan-registry test).
    pub fn with_recorded(mut self, recorded: Vec<RecordedSnapshot>) -> Self {
        self.recorded = recorded;
        self.available
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self
    }

    /// How many times `end_cycle` was called (release accounting).
    pub fn end_cycle_calls(&self) -> usize {
        self.end_cycle_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many times `map_for_volume` was called.
    pub fn map_calls(&self) -> usize {
        self.map_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many times `recorded_snapshots` was called (P1-2 per-source record
    /// accounting).
    pub fn recorded_calls(&self) -> usize {
        self.recorded_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl VssProvider for FakeVssProvider {
    fn map_for_volume(&self, live_path: &Path) -> SnapshotOutcome {
        self.map_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match &self.mapped_root {
            Some(root) => {
                // P1-A: a successful map simulates a created shadow, so fire the
                // record-at-create spy synchronously here (before returning the
                // mapped path) exactly as the real provider does at create time.
                let hook = match self.recorder.lock() {
                    Ok(g) => g.clone(),
                    Err(poisoned) => poisoned.into_inner().clone(),
                };
                if let Some(hook) = hook {
                    hook(&self.recorder_guid, 0);
                }
                let name = live_path
                    .file_name()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default();
                SnapshotOutcome::Mapped(root.join(name))
            }
            None => SnapshotOutcome::Unavailable,
        }
    }

    fn set_recorder(&self, recorder: SnapshotRecorder) {
        match self.recorder.lock() {
            Ok(mut g) => *g = Some(recorder),
            Err(poisoned) => *poisoned.into_inner() = Some(recorder),
        }
    }

    fn mode(&self) -> VssMode {
        match self.mode.lock() {
            Ok(g) => *g,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn set_mode(&self, mode: VssMode) {
        match self.mode.lock() {
            Ok(mut g) => *g = mode,
            Err(poisoned) => *poisoned.into_inner() = mode,
        }
    }

    fn available(&self) -> bool {
        // Mirror the real provider: `never` is never available, regardless of
        // the constructed `available` flag, so the P1-5 `set_mode(Never)` test
        // observes the same short-circuit the executor relies on.
        self.mode() != VssMode::Never && self.available.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn end_cycle(&self) {
        self.end_cycle_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn recorded_snapshots(&self) -> Vec<RecordedSnapshot> {
        self.recorded_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.recorded.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn unavailable_fake_degrades_and_counts_end_cycle() {
        let p = FakeVssProvider::unavailable();
        assert!(!p.available());
        assert_eq!(p.mode(), VssMode::Auto);
        assert_eq!(
            p.map_for_volume(Path::new("/some/file.pst")),
            SnapshotOutcome::Unavailable
        );
        assert_eq!(p.map_calls(), 1);
        p.end_cycle();
        p.end_cycle();
        assert_eq!(p.end_cycle_calls(), 2);
    }

    #[test]
    fn mapped_fake_returns_mapped_path() {
        let p = FakeVssProvider::mapped_under(VssMode::Always, "/snap/root");
        assert!(p.available());
        assert_eq!(p.mode(), VssMode::Always);
        assert_eq!(
            p.map_for_volume(Path::new("/live/dir/outlook.pst")),
            SnapshotOutcome::Mapped(PathBuf::from("/snap/root/outlook.pst"))
        );
    }

    /// P1-A: a record-at-create hook installed via `set_recorder` fires
    /// SYNCHRONOUSLY when `map_for_volume` produces a snapshot (the fake's
    /// `Mapped` outcome simulates a created shadow), BEFORE the mapped path is
    /// returned - so the GUID is recorded the instant the shadow exists. An
    /// `Unavailable` provider (no shadow created) never fires it.
    #[test]
    fn recorder_fires_synchronously_at_create() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let seen_guid = Arc::new(Mutex::new(String::new()));

        // Available provider that maps (= creates) a shadow.
        let p = FakeVssProvider::mapped_under(VssMode::Always, "/snap/root");
        let calls_c = calls.clone();
        let guid_c = seen_guid.clone();
        p.set_recorder(Arc::new(move |guid: &str, _created: i64| {
            calls_c.fetch_add(1, Ordering::SeqCst);
            *guid_c.lock().unwrap() = guid.to_string();
        }));

        let out = p.map_for_volume(Path::new("/live/dir/outlook.pst"));
        assert!(matches!(out, SnapshotOutcome::Mapped(_)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the recorder must fire exactly once at create time"
        );
        assert!(
            !seen_guid.lock().unwrap().is_empty(),
            "the recorder must receive the snapshot GUID"
        );

        // An UNAVAILABLE provider creates no shadow, so the recorder is silent.
        let calls2 = Arc::new(AtomicUsize::new(0));
        let u = FakeVssProvider::unavailable();
        let calls2_c = calls2.clone();
        u.set_recorder(Arc::new(move |_g: &str, _c: i64| {
            calls2_c.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(
            u.map_for_volume(Path::new("/some/file.pst")),
            SnapshotOutcome::Unavailable
        );
        assert_eq!(
            calls2.load(Ordering::SeqCst),
            0,
            "no shadow created => recorder must not fire"
        );
    }

    #[test]
    fn real_provider_off_windows_or_unelevated_is_unavailable() {
        // Off Windows this is always unavailable; on an un-elevated Windows
        // dev box / CI runner it is also unavailable. Either way the contract
        // is: no panic, degrade.
        let p = RealVssProvider::new(VssMode::Auto);
        if !p.available() {
            assert_eq!(
                p.map_for_volume(Path::new("C:\\Users\\x\\f.pst")),
                SnapshotOutcome::Unavailable
            );
        }
        // end_cycle is always safe to call.
        p.end_cycle();
    }

    #[test]
    fn real_provider_never_mode_is_unavailable_even_if_elevated() {
        let p = RealVssProvider::new(VssMode::Never);
        assert!(!p.available(), "never mode is never available");
        p.end_cycle();
    }

    #[test]
    fn real_provider_set_mode_never_disables_and_back_restores() {
        // P1-5: applying `never` via set_mode must make the provider report
        // unavailable even when it was constructed in another mode; restoring
        // a non-never mode lets `available` follow elevation/OS again.
        let p = RealVssProvider::new(VssMode::Auto);
        p.set_mode(VssMode::Never);
        assert_eq!(p.mode(), VssMode::Never);
        assert!(!p.available(), "set_mode(Never) must disable the provider");
        p.set_mode(VssMode::Always);
        assert_eq!(p.mode(), VssMode::Always);
        // On a non-Windows / un-elevated runner this is still unavailable for
        // OTHER reasons; the invariant we assert is only "never -> unavailable".
        let _ = p.available();
    }

    #[test]
    fn fake_provider_set_mode_never_reports_unavailable() {
        // P1-5: the fake mirrors the real provider so an orchestrator test can
        // drive `set_mode(Never) -> available() == false` without real COM.
        let p = FakeVssProvider::mapped_under(VssMode::Always, "/snap/root");
        assert!(p.available());
        p.set_mode(VssMode::Never);
        assert_eq!(p.mode(), VssMode::Never);
        assert!(
            !p.available(),
            "set_mode(Never) on the fake must report unavailable"
        );
    }
}
