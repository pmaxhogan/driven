//! Filesystem-watcher surface: per-source change detection that triggers
//! an early scan-tick (DESIGN s5.9).
//!
//! The watcher is a *latency win* layered on top of the authoritative
//! scheduled scan, never a replacement: a missed watcher event must only
//! make backup *slower*, never silently *lose* a file (DESIGN s5.9.5). So
//! a watcher event is a **scan-tick request**, not a per-file upload
//! trigger (DESIGN s5.9.1) - the scanner remains the diff-and-plan engine.
//!
//! This module is the I/O-free contract. The real watcher uses `notify`
//! v8 (inotify / FSEvents / ReadDirectoryChangesW); that crate lives in
//! `[workspace.dependencies]` and is wired into the implementer crate,
//! keeping `driven-core` I/O-free (lib.rs). Tests drive the trait directly.

use crate::types::SourceId;

/// Why the watcher emitted a [`ScanTickRequest`] (DESIGN s5.9).
///
/// The orchestrator treats `Edit` as "scan earlier than the next
/// scheduled tick" and `WatcherDied` / `Degraded` as "fall back to
/// scheduled-only for this source and try to re-establish the watch on
/// the next tick" (DESIGN s5.9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanTickReason {
    /// One or more debounced filesystem edits were observed under the
    /// source root (DESIGN s5.9.2: collapsed after a 500 ms quiet window,
    /// capped at one request/minute/source from the watcher).
    Edit,
    /// The watcher's backend reported a recoverable degradation (inotify
    /// watch-limit exhausted, FSEvents coalescing, a Windows
    /// directory-handle invalidation) and the source is now relying on the
    /// scheduled scan until the watch is re-established (DESIGN s5.9.4).
    Degraded,
    /// The watcher thread / channel closed unexpectedly; the orchestrator
    /// logs `WatcherDied`, falls back to scheduled-only, and attempts a
    /// restart on the next tick (DESIGN s5.9.4).
    WatcherDied,
}

/// A debounced request from the watcher asking the orchestrator to scan a
/// source ahead of its scheduled tick (DESIGN s5.9.1, s5.9.2).
///
/// Carries no per-file path list by design: the watcher says only "scan
/// this source"; the scanner re-derives the actual diff (DESIGN s5.9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScanTickRequest {
    /// Source the early scan is requested for.
    pub source_id: SourceId,
    /// Why the request was emitted.
    pub reason: ScanTickReason,
}

/// The per-source filesystem-watcher contract (DESIGN s5.9).
///
/// One implementation watches one source's `local_path` recursively and
/// emits debounced [`ScanTickRequest`]s. The orchestrator consumes them
/// over a `tokio::sync::mpsc` channel (DESIGN s5.9.1); the trait exposes
/// that subscription plus lifecycle control so a source can be
/// re-watched after a `Degraded` / `WatcherDied` event.
pub trait SourceWatcher: Send + Sync {
    /// Begins watching the source root recursively. Idempotent: calling it
    /// on an already-watching source re-establishes the watch (the restart
    /// path after a degradation, DESIGN s5.9.4). Returns an error if the
    /// backend cannot establish the watch at all (e.g. the path no longer
    /// exists); a *recoverable* degradation is reported in-band as a
    /// [`ScanTickReason::Degraded`] request instead.
    fn watch(&self, source_id: SourceId) -> anyhow::Result<()>;

    /// Stops watching the source and releases the backend handle.
    fn unwatch(&self, source_id: SourceId);

    /// Returns the receive end of the debounced scan-tick stream
    /// (DESIGN s5.9.1: a single mpsc consumer - the orchestrator selects on
    /// it alongside its scheduled-tick timer and the power / network event
    /// channels).
    ///
    /// **Take-once semantics.** `mpsc::Receiver` is single-consumer and not
    /// `Clone`, so this hands out the one receiver and an implementation
    /// may return it at most once (a second call returns `None`). The
    /// implement phase may instead choose to hand the receiver to the
    /// orchestrator at construction and keep only the `Sender` behind the
    /// trait; that reshaping is in-bounds (see the M3 phase-1 finding).
    fn subscribe(&self) -> Option<tokio::sync::mpsc::Receiver<ScanTickRequest>>;
}

use serde::{Deserialize, Serialize};
