//! Below-normal CPU + I/O priority for backup work (SPEC s22 `io_priority`).
//!
//! Driven is a background backup tool: it should not fight the foreground
//! applications the user is actually looking at. The `global.io_priority`
//! setting (`normal` | `low` | `idle`) selects how far below normal the
//! threads that do backup work run, and this module is the one place that
//! translates that setting into real per-thread OS calls.
//!
//! # Everything here is best-effort
//!
//! Priority is an optimisation, never correctness. Every OS call is fired and
//! its failure logged at `debug`; nothing returns a `Result`, nothing panics,
//! and nothing aborts a backup. A kernel that refuses the call (an unusual
//! sandbox, a container with no `ioprio_set`, an old Windows Server) simply
//! runs the work at normal priority - fail-working, never fail-erroring.
//!
//! # Per-thread, and ONLY on threads that cannot yield
//!
//! Every mechanism used here is **per-thread**, and the threads doing backup
//! work are pooled and reused (`tokio::task::spawn_blocking` hands its threads
//! to the next task when the closure returns). A thread left demoted after the
//! backup work finished would silently throttle whatever ran on it next -
//! including UI/IPC work. Two rules fall out of that, and both are enforced by
//! the types rather than by convention:
//!
//! 1. [`begin_background_work`] returns a [`PriorityGuard`] that restores the
//!    thread on drop, undoing exactly what it managed to apply.
//! 2. [`PriorityGuard`] is deliberately **not** [`Send`]. Holding one across an
//!    `.await` would let the task resume on a different thread, demoting an
//!    innocent thread and leaking the demotion on the original one. Making the
//!    guard `!Send` turns that mistake into a compile error inside any spawned
//!    task instead of a runtime mystery.
//!
//! The practical consequence is that a guard belongs inside a `spawn_blocking`
//! closure (or any `fn` with no `.await`), not wrapped around an async block.
//! [`spawn_blocking`] packages the common case.
//!
//! [`apply_to_current_thread`] is the one-shot escape hatch for threads Driven
//! owns for their entire life (a dedicated walker/hasher worker), where there
//! is no "afterwards" to restore to.
//!
//! # Where this is applied today, and what it does NOT cover yet
//!
//! One site: the executor's `build_bundle` blocking task (V2 small-file
//! bundling), which reads every member off disk and gzips it. So the setting
//! currently shapes **bundled small-file uploads only**.
//!
//! A large-file backup is not covered, and cannot be by a per-thread guard as
//! things stand: the read/hash/encrypt/upload pipeline is a set of interleaved
//! async stages, and the actual disk reads happen on `tokio::fs`'s internal
//! blocking pool, which Driven does not own a handle to. The scanner's walk and
//! deep-verify hashing are the other big consumers, and they run inline on the
//! async task today. The win there arrives when the scanner moves to dedicated
//! worker threads: those are Driven's own, live for the whole walk, and should
//! call [`apply_to_current_thread`] at startup. This module exists in the shape
//! it does so that adoption is a one-line change.
//!
//! # What each level maps to, per OS
//!
//! | | Windows | Linux | macOS |
//! |---|---|---|---|
//! | `Low` | `THREAD_PRIORITY_BELOW_NORMAL` (CPU only) | `ioprio` best-effort 6 (I/O only) | `IOPOL_UTILITY` disk policy (I/O only) |
//! | `Idle` | `THREAD_MODE_BACKGROUND_BEGIN` (CPU + I/O + memory) | `ioprio` idle class (I/O only) | `IOPOL_THROTTLE` + `PRIO_DARWIN_BG` (CPU + I/O) |
//!
//! The gaps are real and deliberate, not oversights:
//!
//! - **Windows `Low` lowers CPU only.** The documented per-thread I/O priority
//!   hint is `THREAD_MODE_BACKGROUND_BEGIN`, which is all-or-nothing: it drops
//!   CPU, I/O *and* memory priority to the background band in one call. That is
//!   the `Idle` behaviour, so `Low` gets the CPU notch alone
//!   (`THREAD_PRIORITY_BELOW_NORMAL`) - precisely "one notch below normal".
//! - **Linux does not change the nice value inside a guard.** `setpriority`
//!   is a one-way ratchet for an unprivileged process: raising the nice value
//!   always succeeds, lowering it back needs `CAP_SYS_NICE` or a raised
//!   `RLIMIT_NICE` (soft limit 0 on most distributions), so the restore would
//!   fail with `EACCES` and leave a pooled thread deniced forever. Only the
//!   reversible `ioprio_set` runs inside a guard; the nice bump is applied by
//!   [`apply_to_current_thread`], which makes no restore promise.
//!
//! # Live settings changes
//!
//! The current level lives in a [`PriorityCell`] - one `Arc<AtomicU8>` shared
//! by the app assembly into both the orchestrator and the executor, the same
//! one-Arc-into-two-consumers pattern as the pacer and the upload pool. The
//! orchestrator writes the cell on `reconfigure`, so a settings save is picked
//! up by work that starts after it; work already in flight keeps the level it
//! started with.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

const TARGET: &str = "driven::priority";

/// How far below normal a backup-work thread should run (SPEC s22
/// `global.io_priority`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPriority {
    /// No demotion - backup work competes with everything else on equal terms.
    #[default]
    Normal,
    /// One notch below normal: enough to yield to foreground applications,
    /// not so low that a busy machine starves the backup.
    Low,
    /// Only run when the machine is otherwise idle.
    Idle,
}

impl WorkPriority {
    /// Parse the persisted setting string, tolerating case and surrounding
    /// whitespace. An unknown value degrades to [`WorkPriority::Normal`] rather
    /// than erroring - the settings layer already validates the enum, so
    /// reaching this branch means the blob was hand-edited or written by a
    /// newer version.
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "idle" => Self::Idle,
            _ => Self::Normal,
        }
    }

    /// The persisted setting string for this level (the inverse of
    /// [`Self::from_setting`]).
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Low => "low",
            Self::Idle => "idle",
        }
    }

    /// Compact representation for [`PriorityCell`]'s atomic.
    fn to_u8(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Low => 1,
            Self::Idle => 2,
        }
    }

    /// Inverse of [`Self::to_u8`]; an out-of-range byte degrades to
    /// [`WorkPriority::Normal`] (unreachable in practice - only `to_u8` writes
    /// the cell).
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Low,
            2 => Self::Idle,
            _ => Self::Normal,
        }
    }
}

/// The live [`WorkPriority`], shared between the orchestrator (which writes it
/// on a settings change) and the executor (which reads it when it starts a
/// piece of blocking work).
///
/// Cheap to clone - it is an `Arc` over one atomic byte - and read at the guard
/// site rather than captured at construction, so a settings save takes effect
/// on the next piece of work without restarting anything.
#[derive(Debug, Clone, Default)]
pub struct PriorityCell(Arc<AtomicU8>);

impl PriorityCell {
    /// A cell holding `priority`.
    #[must_use]
    pub fn new(priority: WorkPriority) -> Self {
        Self(Arc::new(AtomicU8::new(priority.to_u8())))
    }

    /// The current level.
    #[must_use]
    pub fn get(&self) -> WorkPriority {
        WorkPriority::from_u8(self.0.load(Ordering::Relaxed))
    }

    /// Replace the current level. Seen by the next [`begin_background_work`]
    /// call; work already running keeps the level it started with.
    pub fn set(&self, priority: WorkPriority) {
        self.0.store(priority.to_u8(), Ordering::Relaxed);
    }
}

/// Exactly what [`apply`] managed to do to the calling thread, so the restore
/// undoes that and only that.
///
/// Recording the *outcome* rather than the *intent* matters: on Windows,
/// `THREAD_MODE_BACKGROUND_END` fails with `ERROR_THREAD_MODE_NOT_BACKGROUND`
/// when no matching begin succeeded, and on every OS a refused call must not
/// produce a bogus restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Applied {
    /// Windows: the thread priority to hand back to `SetThreadPriority`,
    /// recorded before it was lowered.
    #[cfg(windows)]
    restore_thread_priority: Option<i32>,
    /// Windows: a `THREAD_MODE_BACKGROUND_BEGIN` that actually succeeded, and
    /// therefore owes a `THREAD_MODE_BACKGROUND_END`.
    #[cfg(windows)]
    end_background_mode: bool,
    /// Linux: an `ioprio_set` that succeeded, and therefore owes a reset to the
    /// default scheduling class.
    #[cfg(target_os = "linux")]
    reset_ioprio: bool,
    /// macOS: the disk I/O policy to hand back to `setiopolicy_np`, read before
    /// it was lowered.
    #[cfg(target_os = "macos")]
    restore_iopolicy: Option<i32>,
    /// macOS: a `PRIO_DARWIN_BG` that succeeded, and therefore owes a clear.
    #[cfg(target_os = "macos")]
    clear_darwin_bg: bool,
}

/// Restores the calling thread's priority when dropped (RAII).
///
/// Returned by [`begin_background_work`]. Deliberately **not** [`Send`]: the
/// restore has to happen on the same thread that was demoted, so the guard must
/// not be able to travel to another one. That also makes any future holding it
/// non-`Send`, which turns "held across an `.await` inside a spawned task" into
/// a compile error - see the module docs.
pub struct PriorityGuard {
    applied: Applied,
    /// The `!Send` marker. A raw pointer is the standard way to opt a type out
    /// of `Send`/`Sync` without a nightly negative impl.
    _not_send: PhantomData<*const ()>,
}

impl Drop for PriorityGuard {
    fn drop(&mut self) {
        restore(self.applied);
    }
}

/// Demote the calling thread to `priority` for as long as the returned guard
/// lives (the guard restores it on drop).
///
/// Best-effort in every direction: a refused call is logged at `debug` and the
/// work simply runs at normal priority. [`WorkPriority::Normal`] applies
/// nothing at all, so the whole mechanism costs one branch when the user leaves
/// the setting alone.
///
/// Call this INSIDE a blocking closure, not around an async block - see the
/// module docs for why (and note that the `!Send` guard enforces it).
#[must_use = "the thread is restored to normal priority as soon as the guard drops"]
pub fn begin_background_work(priority: WorkPriority) -> PriorityGuard {
    PriorityGuard {
        applied: apply(priority),
        _not_send: PhantomData,
    }
}

/// Demote the calling thread to `priority` permanently, with no restore.
///
/// For threads Driven owns end to end - a dedicated walker or hasher worker
/// that exists only to do backup work and is never handed back to a shared
/// pool. On a pooled thread use [`begin_background_work`] instead.
///
/// Because there is no restore to keep honest, this is also where Linux applies
/// its nice bump (`+5` for [`WorkPriority::Low`], `+10` for
/// [`WorkPriority::Idle`]): raising the nice value of an unprivileged thread is
/// permitted but irreversible, which is exactly the contract of this function
/// and exactly not the contract of the guard.
pub fn apply_to_current_thread(priority: WorkPriority) {
    let _applied = apply(priority);
    #[cfg(target_os = "linux")]
    linux::set_nice(match priority {
        WorkPriority::Normal => return,
        WorkPriority::Low => 5,
        WorkPriority::Idle => 10,
    });
}

/// Run `f` on a `tokio` blocking thread with the thread demoted to `priority`
/// for the duration, restoring it before the thread returns to the pool.
///
/// The ergonomic wrapper over `tokio::task::spawn_blocking` +
/// [`begin_background_work`] for the executor's blocking sections; identical to
/// `spawn_blocking` in every other respect (same `JoinHandle`, same panic
/// propagation).
pub fn spawn_blocking<F, R>(priority: WorkPriority, f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _guard = begin_background_work(priority);
        f()
    })
}

/// Apply `priority` to the calling thread, returning what actually took effect.
fn apply(priority: WorkPriority) -> Applied {
    if priority == WorkPriority::Normal {
        return Applied::default();
    }
    platform_apply(priority)
}

#[cfg(windows)]
fn platform_apply(priority: WorkPriority) -> Applied {
    windows_impl::apply(priority)
}

#[cfg(target_os = "linux")]
fn platform_apply(priority: WorkPriority) -> Applied {
    linux::apply(priority)
}

#[cfg(target_os = "macos")]
fn platform_apply(priority: WorkPriority) -> Applied {
    macos::apply(priority)
}

/// Every other target (the BSDs, and anything Driven has not been ported to)
/// runs backup work at normal priority rather than failing.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_apply(priority: WorkPriority) -> Applied {
    tracing::debug!(
        target: TARGET,
        ?priority,
        "no thread-priority backend for this platform; running at normal priority"
    );
    Applied::default()
}

/// Undo exactly what [`apply`] recorded.
#[cfg(windows)]
fn restore(applied: Applied) {
    windows_impl::restore(applied);
}

#[cfg(target_os = "linux")]
fn restore(applied: Applied) {
    linux::restore(applied);
}

#[cfg(target_os = "macos")]
fn restore(applied: Applied) {
    macos::restore(applied);
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn restore(_applied: Applied) {}

// -----------------------------------------------------------------------------
// Windows
// -----------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::{Applied, WorkPriority, TARGET};

    // Win32 surface used here, declared locally rather than pulling the
    // `windows` crate into driven-core for three calls (the same approach the
    // scanner's `FindFirstStreamW` ADS probe takes).
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(thread: isize, priority: i32) -> i32;
        fn GetThreadPriority(thread: isize) -> i32;
    }

    /// One CPU notch below the process priority class.
    pub(super) const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
    /// The priority every thread starts at.
    pub(super) const THREAD_PRIORITY_NORMAL: i32 = 0;
    /// Enter background processing mode: CPU, I/O and memory priority all drop
    /// to the background band. Only valid for the CURRENT thread, and it fails
    /// if the thread is already in background mode.
    pub(super) const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
    /// Leave background processing mode. Fails if the thread is not in it.
    pub(super) const THREAD_MODE_BACKGROUND_END: i32 = 0x0002_0000;
    /// What `GetThreadPriority` returns when it fails.
    const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;

    /// The calling thread's current priority, or `None` if the read failed.
    pub(super) fn current_thread_priority() -> Option<i32> {
        // SAFETY: both calls take no pointers. `GetCurrentThread` returns a
        // pseudo-handle to the calling thread that needs no closing.
        let value = unsafe { GetThreadPriority(GetCurrentThread()) };
        (value != THREAD_PRIORITY_ERROR_RETURN).then_some(value)
    }

    /// Set the calling thread's priority (or background mode); `true` on
    /// success.
    pub(super) fn set_thread_priority(priority: i32) -> bool {
        // SAFETY: as above - no pointers, and the pseudo-handle is the current
        // thread, which is what BACKGROUND_BEGIN/END require.
        unsafe { SetThreadPriority(GetCurrentThread(), priority) != 0 }
    }

    pub(super) fn apply(priority: WorkPriority) -> Applied {
        let mut applied = Applied::default();
        match priority {
            WorkPriority::Normal => {}
            WorkPriority::Low => {
                // CPU only. The documented per-thread I/O hint is background
                // mode, which also floors CPU + memory priority - that is the
                // Idle level, not "one notch below normal".
                let previous = current_thread_priority();
                if set_thread_priority(THREAD_PRIORITY_BELOW_NORMAL) {
                    applied.restore_thread_priority =
                        Some(previous.unwrap_or(THREAD_PRIORITY_NORMAL));
                } else {
                    tracing::debug!(
                        target: TARGET,
                        error = %std::io::Error::last_os_error(),
                        "SetThreadPriority(BELOW_NORMAL) refused; running at normal priority"
                    );
                }
            }
            WorkPriority::Idle => {
                if set_thread_priority(THREAD_MODE_BACKGROUND_BEGIN) {
                    applied.end_background_mode = true;
                } else {
                    // ERROR_THREAD_MODE_ALREADY_BACKGROUND lands here too: some
                    // outer scope owns the background mode and owes the END, so
                    // this guard must NOT issue one.
                    tracing::debug!(
                        target: TARGET,
                        error = %std::io::Error::last_os_error(),
                        "SetThreadPriority(BACKGROUND_BEGIN) refused; running at normal priority"
                    );
                }
            }
        }
        applied
    }

    pub(super) fn restore(applied: Applied) {
        if applied.end_background_mode && !set_thread_priority(THREAD_MODE_BACKGROUND_END) {
            tracing::debug!(
                target: TARGET,
                error = %std::io::Error::last_os_error(),
                "SetThreadPriority(BACKGROUND_END) failed; thread may stay in background mode"
            );
        }
        if let Some(previous) = applied.restore_thread_priority {
            if !set_thread_priority(previous) {
                tracing::debug!(
                    target: TARGET,
                    error = %std::io::Error::last_os_error(),
                    "restoring the thread priority failed; thread may stay demoted"
                );
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Linux
// -----------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::{Applied, WorkPriority, TARGET};

    /// `who` is a thread id; `0` means the calling thread.
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    /// Best-effort class - the default class, priority 0 (highest) .. 7.
    const IOPRIO_CLASS_BE: libc::c_int = 2;
    /// Idle class - only serviced when nothing else wants the disk.
    const IOPRIO_CLASS_IDLE: libc::c_int = 3;
    /// The class occupies the bits above the priority data.
    const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
    /// Within best-effort: near the bottom of 0..=7, but not the idle class.
    const IOPRIO_BE_LOW: libc::c_int = 6;
    /// Since Linux 2.6.24, an ioprio of 0 resets the thread to the kernel's
    /// default I/O scheduling behaviour.
    const IOPRIO_DEFAULT: libc::c_int = 0;

    /// `IOPRIO_PRIO_VALUE(class, data)` from `linux/ioprio.h`.
    const fn ioprio_value(class: libc::c_int, data: libc::c_int) -> libc::c_int {
        (class << IOPRIO_CLASS_SHIFT) | data
    }

    /// `ioprio_set(IOPRIO_WHO_PROCESS, 0, ioprio)` on the calling thread;
    /// `true` on success.
    fn set_ioprio(ioprio: libc::c_int) -> bool {
        // SAFETY: a plain scalar syscall - no pointers, and `who = 0` scopes it
        // to the calling thread. The arguments are widened to `c_long` because
        // `syscall` is variadic and the kernel reads register-width words.
        unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS as libc::c_long,
                0 as libc::c_long,
                ioprio as libc::c_long,
            ) == 0
        }
    }

    /// Raise the calling thread's nice value. IRREVERSIBLE for an unprivileged
    /// process, so this is only reachable from `apply_to_current_thread`.
    pub(super) fn set_nice(nice: libc::c_int) {
        // SAFETY: no pointers; `who = 0` with PRIO_PROCESS is the calling
        // thread (Linux nice values are per-thread). The `as _` on `which`
        // absorbs the per-libc difference between `c_int` and `c_uint`.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS as _, 0, nice) } != 0 {
            tracing::debug!(
                target: TARGET,
                error = %std::io::Error::last_os_error(),
                nice,
                "setpriority refused; thread keeps its current nice value"
            );
        }
    }

    pub(super) fn apply(priority: WorkPriority) -> Applied {
        // Only the I/O class is touched inside a guard: it is the reversible
        // half. The nice value is a one-way ratchet without CAP_SYS_NICE (see
        // the module docs), so raising it here would leave a pooled thread
        // deniced for the rest of the process's life.
        let ioprio = match priority {
            WorkPriority::Normal => return Applied::default(),
            WorkPriority::Low => ioprio_value(IOPRIO_CLASS_BE, IOPRIO_BE_LOW),
            WorkPriority::Idle => ioprio_value(IOPRIO_CLASS_IDLE, 0),
        };
        if set_ioprio(ioprio) {
            Applied { reset_ioprio: true }
        } else {
            tracing::debug!(
                target: TARGET,
                error = %std::io::Error::last_os_error(),
                "ioprio_set refused; running at the default I/O priority"
            );
            Applied::default()
        }
    }

    pub(super) fn restore(applied: Applied) {
        if applied.reset_ioprio && !set_ioprio(IOPRIO_DEFAULT) {
            tracing::debug!(
                target: TARGET,
                error = %std::io::Error::last_os_error(),
                "resetting ioprio failed; thread may stay at a lowered I/O priority"
            );
        }
    }
}

// -----------------------------------------------------------------------------
// macOS
// -----------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use super::{Applied, WorkPriority, TARGET};

    // `setiopolicy_np` / `getiopolicy_np` live in libSystem but are not bound
    // by the `libc` crate, so declare them here (same local-declaration
    // approach as the Windows block above).
    extern "C" {
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
        fn getiopolicy_np(iotype: libc::c_int, scope: libc::c_int) -> libc::c_int;
    }

    /// I/O to local disks and remote volumes.
    const IOPOL_TYPE_DISK: libc::c_int = 0;
    /// Scope the policy to the calling thread.
    const IOPOL_SCOPE_THREAD: libc::c_int = 1;
    /// Short-running background work: throttled enough to stay out of the way.
    const IOPOL_UTILITY: libc::c_int = 4;
    /// Long-running I/O-intensive background work - Apple names backups as the
    /// example use case.
    const IOPOL_THROTTLE: libc::c_int = 3;
    /// Unrestricted - what a fresh thread starts at.
    const IOPOL_IMPORTANT: libc::c_int = 1;
    /// `setpriority(PRIO_DARWIN_THREAD, 0, PRIO_DARWIN_BG)` puts the calling
    /// thread in the background band; passing `0` clears it again.
    const PRIO_DARWIN_THREAD: libc::c_int = 3;
    const PRIO_DARWIN_BG: libc::c_int = 0x1000;

    /// The calling thread's current disk I/O policy, or `None` if the read
    /// failed (`getiopolicy_np` reports errors as `-1`).
    fn current_iopolicy() -> Option<libc::c_int> {
        // SAFETY: scalar arguments only.
        let value = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD) };
        (value >= 0).then_some(value)
    }

    /// Set the calling thread's disk I/O policy; `true` on success.
    fn set_iopolicy(policy: libc::c_int) -> bool {
        // SAFETY: scalar arguments only; the THREAD scope keeps it to the
        // calling thread.
        unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, policy) == 0 }
    }

    /// Enter (`PRIO_DARWIN_BG`) or leave (`0`) the Darwin background band on
    /// the calling thread; `true` on success.
    fn set_darwin_bg(value: libc::c_int) -> bool {
        // SAFETY: scalar arguments only; `who = 0` with PRIO_DARWIN_THREAD is
        // the calling thread. The `as _` on `which` absorbs the per-libc
        // difference between `c_int` and `c_uint`.
        unsafe { libc::setpriority(PRIO_DARWIN_THREAD as _, 0, value) == 0 }
    }

    pub(super) fn apply(priority: WorkPriority) -> Applied {
        let mut applied = Applied::default();
        let policy = match priority {
            WorkPriority::Normal => return applied,
            WorkPriority::Low => IOPOL_UTILITY,
            WorkPriority::Idle => IOPOL_THROTTLE,
        };
        let previous = current_iopolicy();
        if set_iopolicy(policy) {
            applied.restore_iopolicy = Some(previous.unwrap_or(IOPOL_IMPORTANT));
        } else {
            tracing::debug!(
                target: TARGET,
                error = %std::io::Error::last_os_error(),
                "setiopolicy_np refused; running at the default I/O policy"
            );
        }
        // The Darwin background band also lowers CPU scheduling, so it is the
        // Idle half of the mapping. Unlike Linux's nice value it IS clearable,
        // which is what makes it safe inside a guard.
        if priority == WorkPriority::Idle {
            if set_darwin_bg(PRIO_DARWIN_BG) {
                applied.clear_darwin_bg = true;
            } else {
                tracing::debug!(
                    target: TARGET,
                    error = %std::io::Error::last_os_error(),
                    "setpriority(PRIO_DARWIN_BG) refused; CPU priority unchanged"
                );
            }
        }
        applied
    }

    pub(super) fn restore(applied: Applied) {
        if applied.clear_darwin_bg && !set_darwin_bg(0) {
            tracing::debug!(
                target: TARGET,
                error = %std::io::Error::last_os_error(),
                "clearing PRIO_DARWIN_BG failed; thread may stay in the background band"
            );
        }
        if let Some(previous) = applied.restore_iopolicy {
            if !set_iopolicy(previous) {
                tracing::debug!(
                    target: TARGET,
                    error = %std::io::Error::last_os_error(),
                    "restoring the disk I/O policy failed; thread may stay throttled"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_setting_maps_the_three_spec_values() {
        assert_eq!(WorkPriority::from_setting("normal"), WorkPriority::Normal);
        assert_eq!(WorkPriority::from_setting("low"), WorkPriority::Low);
        assert_eq!(WorkPriority::from_setting("idle"), WorkPriority::Idle);
    }

    #[test]
    fn from_setting_is_lenient_about_case_and_whitespace() {
        assert_eq!(WorkPriority::from_setting("  LOW "), WorkPriority::Low);
        assert_eq!(WorkPriority::from_setting("Idle"), WorkPriority::Idle);
    }

    #[test]
    fn from_setting_degrades_unknown_values_to_normal() {
        // Fail-working: a hand-edited or newer-version blob must not demote the
        // backup to a level we do not understand.
        assert_eq!(WorkPriority::from_setting("realtime"), WorkPriority::Normal);
        assert_eq!(WorkPriority::from_setting(""), WorkPriority::Normal);
    }

    #[test]
    fn as_setting_round_trips_through_from_setting() {
        for level in [WorkPriority::Normal, WorkPriority::Low, WorkPriority::Idle] {
            assert_eq!(WorkPriority::from_setting(level.as_setting()), level);
        }
    }

    #[test]
    fn default_is_normal_so_an_unwired_build_never_demotes() {
        assert_eq!(WorkPriority::default(), WorkPriority::Normal);
        assert_eq!(PriorityCell::default().get(), WorkPriority::Normal);
    }

    #[test]
    fn cell_shares_one_value_across_clones() {
        let cell = PriorityCell::new(WorkPriority::Normal);
        let clone = cell.clone();
        cell.set(WorkPriority::Idle);
        assert_eq!(clone.get(), WorkPriority::Idle);
        clone.set(WorkPriority::Low);
        assert_eq!(cell.get(), WorkPriority::Low);
    }

    #[test]
    fn normal_applies_nothing_and_drops_cleanly() {
        let guard = begin_background_work(WorkPriority::Normal);
        assert_eq!(guard.applied, Applied::default());
        drop(guard);
    }

    /// Every level must survive an apply/restore round trip on the host OS
    /// without panicking, whatever the kernel decides to allow. This is the
    /// fail-working contract, and it is the only assertion that can be made
    /// portably (Linux/macOS expose no unprivileged read-back of what the
    /// guard changed; the Windows assertions below are the real check).
    #[test]
    fn every_level_applies_and_restores_without_panicking() {
        for level in [WorkPriority::Normal, WorkPriority::Low, WorkPriority::Idle] {
            // On its own thread so a kernel that refuses the restore cannot
            // leak a demotion into the rest of the test binary.
            std::thread::spawn(move || {
                let _guard = begin_background_work(level);
            })
            .join()
            .expect("priority round trip must not panic");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_low_lowers_cpu_priority_and_the_guard_restores_it() {
        use windows_impl::{
            current_thread_priority, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL,
        };

        std::thread::spawn(|| {
            assert_eq!(
                current_thread_priority(),
                Some(THREAD_PRIORITY_NORMAL),
                "a fresh thread starts at THREAD_PRIORITY_NORMAL"
            );
            {
                let _guard = begin_background_work(WorkPriority::Low);
                assert_eq!(
                    current_thread_priority(),
                    Some(THREAD_PRIORITY_BELOW_NORMAL),
                    "Low must lower the thread one CPU notch"
                );
            }
            assert_eq!(
                current_thread_priority(),
                Some(THREAD_PRIORITY_NORMAL),
                "dropping the guard must hand the thread back at normal priority"
            );
        })
        .join()
        .expect("windows Low round trip must not panic");
    }

    /// `GetThreadPriority` does not report background mode (it keeps returning
    /// the saved priority), so the observable proof that the guard issued its
    /// `THREAD_MODE_BACKGROUND_END` is that a FRESH `BACKGROUND_BEGIN`
    /// succeeds: Windows fails that call with
    /// `ERROR_THREAD_MODE_ALREADY_BACKGROUND` while the thread is still in
    /// background mode.
    #[cfg(windows)]
    #[test]
    fn windows_idle_leaves_background_mode_when_the_guard_drops() {
        use windows_impl::{
            set_thread_priority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
        };

        std::thread::spawn(|| {
            drop(begin_background_work(WorkPriority::Idle));
            assert!(
                set_thread_priority(THREAD_MODE_BACKGROUND_BEGIN),
                "the guard must have ended background mode on drop"
            );
            // Leave the thread as we found it (it is about to exit anyway).
            set_thread_priority(THREAD_MODE_BACKGROUND_END);
        })
        .join()
        .expect("windows Idle round trip must not panic");
    }

    #[tokio::test]
    async fn spawn_blocking_runs_the_closure_and_returns_its_value() {
        let out = spawn_blocking(WorkPriority::Low, || 6 * 7)
            .await
            .expect("blocking task must not panic");
        assert_eq!(out, 42);
    }
}
