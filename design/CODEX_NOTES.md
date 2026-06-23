# Codex review notes - residual / deferred items

Durable record of approximations and deferred policy decisions surfaced during
the codex rechecks. Each entry is a known, accepted residual (not a bug), with
the milestone that resolves it.

## M2 nested-gitignore fidelity

The exclude matcher flattens nested `.gitignore` / `.ignore` files into ONE
`GitignoreBuilder` rooted at the source root - an approximation of git's
true per-directory scoping (where a rule in a nested ignore file applies only
under that directory, and a no-slash pattern matches at any depth below its
own file). This was accepted for M2.

Mitigated for data-safety by disabling directory-pruning when the matcher has
any negation (P1-1 fix): a nested `!keep.txt` under an excluded parent dir
would otherwise be classified INCLUDED by the flattened matcher while the
pruned (never-walked) directory left the file un-seen, so the orphan split
would false-classify the file as `deleted` and trash a file that still exists.
With pruning disabled whenever negations exist, the walk filter and the orphan
split decide every path through the same matcher, so they stay consistent and
no still-present file is ever trashed.

True per-directory matching is deferred: it would need the `ignore` crate's
native per-directory matcher stack mirrored for the orphan classification path
(the orphan split must reach the identical decision the walk did for a path
that is NOT currently on disk, so it cannot simply reuse the walker's per-dir
state).

## M2 NFC collision policy

NFC collisions (two byte-distinct raw on-disk paths that normalise onto one
`RelativePath` key, DESIGN s5.2.3 / SPEC s24 `local.unicode_collision`) flow
from the scanner through to `Plan.collisions` (P1-3 plumbing). The scanner
keeps the first-seen file and drops the later collider; M2 does NOT block or
fail on a collision.

The M3 orchestrator owns the user-surfacing + fail-closed policy: it must
surface `local.unicode_collision` as an activity error and decide fail-closed
(block the whole source) vs skip-the-colliding-file-with-an-error.

Recommended: skip the colliding file and surface an error - do NOT block the
whole source. Blocking an entire source's backup over one ambiguous filename is
a disproportionate failure mode; the rest of the source should continue to back
up while the single colliding file is reported for the user to rename.

RESOLVED in M3 (P1-8): the orchestrator now writes a durable `activity_log`
ERROR row per collider (`event_type = local.unicode_collision`, `message =
<colliding path>`, scoped to the source) via `record_collisions`, on the
recommended skip-the-file-not-the-source policy. The source stays visibly
degraded (the error row is surfaced) rather than the collider being silently
skipped.

## M3 codex deferrals

Items surfaced during the M3 codex review (round 1) that are intentionally NOT
fixed at M3. Each is a known, accepted residual with the milestone that
resolves it. The M3 abstractions + seams are in place and tested against the
fakes; the deferred work is the production wiring behind those seams.

### P2-9 production network probe backend (M4)

`network.rs` ships only the probe ABSTRACTION (`NetworkProbe`) + the per-service
circuit breakers, exercised through `FakeNetwork`. There is NO production
reqwest/hickory probe backend wired yet. The orchestrator currently COLLAPSES
the distinct non-online states (`NoInternet` / `DnsFailed` / `CaptivePortal`)
into a single `Paused{Offline}` banner. M4 wires the real reqwest/hickory
backend behind `NetworkProbe` AND preserves the distinct states end-to-end
(so the tray can surface the captive-portal action, the DNS-broken hint, etc.)
rather than flattening them to Offline. M3 acceptance is fake-based; the
abstraction is in place.

### P2-10 per-OS metered detection + reachability (M4)

`driven-power`'s real metered detection is stubbed (always `false`) and
reachability is stubbed (always `true`), so `skip_on_metered` is a NO-OP in
production today (it is exercised at M3 only via `FakePowerSource`). M4 wires the
per-OS backends: Windows `INetworkCostManager`, macOS `NWPath.isExpensive`,
Linux NetworkManager `Metered`. Until then, treat `skip_on_metered` as inert in
a real build.

### CRYPTO SUITE PRODUCTION WIRING (M5/M6 - BEFORE GA)

The executor DOES encrypt content + filenames when constructed with
`ExecutorDeps{ crypto: Some(..) }`, and this is fully tested. But NO production
code path constructs that suite: `DefaultExecutor` is test-only, and the
keystore / master-key -> source-key unwrap must be wired at the app-shell.
This MUST be wired in M5/M6 BEFORE GA - otherwise encryption is INERT in a real
build (an encryption-enabled source would silently upload plaintext because no
suite is threaded in). Flagged as a GA-blocking gap, not a normal deferral.

### 5 e2e rows remain #[ignore]'d as unmeasurable vs the zero-latency fake (M3.7)

Five acceptance rows are quantitative perf/timing claims that cannot be
measured against an instantaneous in-memory fake with no real upload cost,
multi-core timing harness, or real transport. They remain `#[ignore]`d with a
documented reason (NOT faked green) and are to be exercised under the M3.7
latency stress harness:

- `throughput_5x_serial_baseline` - >=5x serial throughput multiplier.
- `blake3_rayon_2x` - blake3 `update_rayon` >=2x single-threaded.
- `adaptive_parallelism_reacts_to_latency` - AIMD parallelism vs real latency.
- `dns_fail_no_hang` - DNS-failure no-hang needs a real transport timeout.
- `lossy_and_intermittent_breaker_cycles` - breaker open/half-open/close cycles
  under real packet loss / intermittent connectivity.

## M3 recheck-2 deferrals

The final M3 codex recheck (round 2) raised two findings folded into existing
deferrals (the real data-loss P1 - timestamps advanced on a failed op - and the
durable per-file failure activity rows were both FIXED in the same commit; these
two are the genuine deferrals):

### Per-source crypto resolution (folds into CRYPTO SUITE PRODUCTION WIRING, M5/M6)

The executor models crypto as one executor-wide `Option<Arc<dyn SourceCryptoSuite>>`
and branches on `self.crypto.is_some()`, but `encryption_enabled` is a PER-SOURCE
setting. A mixed account must not upload an encrypted source plaintext (suite
`None`) nor an unencrypted source as ciphertext (suite `Some`). This cannot
misfire today because no production path constructs the executor with a suite
(encryption is inert until the M5/M6 wiring above). When that wiring lands it MUST
resolve the suite per `SourceRow`/`source_id` (a `CryptoProvider` keyed by source
id), FAIL CLOSED when `encryption_enabled` is true but no key/suite is available,
and force plaintext when it is false. Tracked as part of the same GA-blocking
crypto-wiring task, not a separate fix.

### Drive circuit breaker driven by real request outcomes (folds into P2-9, M4)

`network::CircuitBreaker::note_outcome()` exists but the executor / remote-store
path never calls it, so the Drive breaker (read in `evaluate_gates`) is driven by
probes alone, not by actual upload/update failures. When the real reqwest/hickory
backend is wired in M4 (P2-9 above), thread a request-outcome reporter into the
executor and call `note_outcome(ServiceName::Drive, ok)` on real Drive request
success/failure so the breaker reacts to true request health.

## M3.5 Windows VSS for locked files

The `driven-vss` crate (ROADMAP M3.5, DESIGN s5.3) implements VSS snapshot reads
for exclusively-locked files (Outlook PSTs, running DB files, hypervisor disk
images). The backend (the `VssProvider` seam, `VssMode`, `is_elevated`, the pure
`fallback_decision`, the orphan-cleanup ledger, and the real IVssBackupComponents
COM sequence) landed at M3.5. Below are the deliberate residuals.

### IVssBackupComponents is hand-declared (windows-rs gap, NOT a stub)

The task spec said to use "the `windows` 0.62 `Win32::Storage::Vss`
IVssBackupComponents COM API". That binding DOES NOT EXIST in windows 0.62:
`IVssBackupComponents` and its `CreateVssBackupComponents` factory were never
projected by win32metadata (microsoft/win32metadata#2095, still open as of
2025-06). Verified by the compiler: `use
windows::Win32::Storage::Vss::IVssBackupComponents` fails with E0432 unresolved
import, while the supporting types (`IVssAsync`, `VSS_SNAPSHOT_PROP`, the
`VSS_CTX_*`/`VSS_SS_*` constants, `VSS_BACKUP_TYPE`) DO resolve.

So `crates/driven-vss/src/windows_vss.rs` HAND-DECLARES the `IVssBackupComponents`
vtable with `windows::core::interface`, using the real IID
(`665c1d5f-c218-414d-a05d-7fef5f9d5c86`) and the full 48-method vtable in exact
`vsbackup.h` order (the ~38 methods Driven never calls are placeholder
`_slotNN(&self) -> HRESULT` stubs that only hold their slot offset; only the ~11
called methods carry real signatures). The factory
(`CreateVssBackupComponentsInternal` in `vssapi.dll`, which is what
`CreateVssBackupComponents` resolves to) is loaded via `GetProcAddress` at
runtime. The IID + method order were lifted from `vsbackup.h` (the winapi crate's
RIDL declaration) and cross-checked against MS Learn - NOT reconstructed from
memory, because a wrong slot or IID compiles green and fails only at runtime. The
`windows-core` crate is a direct dependency because the `#[interface]` macro
expands to absolute `::windows_core` paths.

This is correct and complete, NOT a stub. The full DESIGN s5.3 COM sequence runs
(`CoInitializeEx` -> `CreateVssBackupComponents` -> `InitializeForBackup` ->
`SetContext(VSS_CTX_BACKUP)` -> `SetBackupState` -> `GatherWriterMetadata` (async
Wait+QueryStatus) -> `StartSnapshotSet` -> `AddToSnapshotSet` -> `PrepareForBackup`
-> `DoSnapshotSet` -> `GetSnapshotProperties`; `Drop` runs `BackupComplete` +
release). CI verifies COMPILATION only: the VSS path needs Administrator
elevation, which CI lacks, so the `locked_file_backs_up_via_real_vss_snapshot`
integration test honestly gate-skips on `!is_elevated()` (it is NOT
`#[ignore]`-faked) and a local elevated `cargo test` exercises the real COM path.

### Task Scheduler "run elevated on login" one-click - DEFERRED to M5

ROADMAP M3.5 lists a one-click "Set Driven to run elevated on login" action
(`schtasks /create /RL HIGHEST`) plus a "Restart Driven elevated now?" prompt
(`app.restart()` with the elevated entry point). This needs the tray / app-shell,
which does not exist until M5. The backend hooks it wires to (`is_elevated`, the
`VssProvider` degrade path, `vss_mode`) all landed at M3.5; M5 adds the UI action.

### Settings -> Rules -> Windows vss_mode WIDGET + elevation banner - DEFERRED to M6

The `vss_mode` PERSISTED field (SPEC s22 `windows.vss_mode`) and the orchestrator
honouring it landed at M3.5: `OrchestratorConfig.vss_mode: VssMode` (default
`auto`), threaded into the `VssProvider`. The Settings UI WIDGET that edits the
`windows` settings key, and the DESIGN s5.3 elevation banner ("Driven needs to run
elevated to use Volume Shadow Copy..."), need the settings UI, which does not exist
until M6. The `windows` settings key itself is seeded at runtime by the app shell
(per the `0002_seed_settings.sql` comment), not in the global seed.

### Orphan-snapshot cleanup - WIRED end-to-end (one Windows-only edge)

The orphan-cleanup is fully wired at M3.5, NOT deferred. The `OrphanRegistry`
ledger is PERSISTED through `StateRepo::get_setting`/`set_setting` under the
`vss.orphans` settings key (no schema change - it is a JSON value). Each cycle,
after the source loop, the orchestrator records the provider's live shadow GUIDs
+ creation times into the registry (the crash safety net), releases them
in-process via `end_cycle`, then forgets the released GUIDs - so a clean cycle
leaves an empty registry and a `kill -9` between record and forget leaves a
durable entry. On startup (once per process) `cleanup_orphan_snapshots_once`
reads the registry, selects entries older than the >1h cutoff (`prune_orphans`),
and releases each via `VssSnapshot::delete_by_id` (a not-found shadow is an
idempotent no-op). Ownership is PROVEN, never guessed: only recorded GUIDs are
eligible; we never enumerate or heuristically guess. The full round-trip
(record -> release -> forget; pre-seeded-old-orphan selection) is tested at the
orchestrator level on every OS.

The ONE Windows-elevated edge: the actual `DeleteSnapshots` COM call only runs on
elevated Windows. Off Windows / un-elevated, `delete_by_id` returns
`VssError::Unavailable`, so an old recorded orphan is KEPT (never silently
dropped) for a later elevated run to sweep - which is correct. The selection
logic, persistence, and guard are exercised cross-OS; only the final COM deletion
needs the elevated Windows runtime (same constraint as the integration test).

### Blocking COM on a tokio worker - ACCEPTED for V1

`VssProvider::map_for_volume` runs the synchronous snapshot creation (DESIGN s5.3
budgets up to ~10s) inline on the executor's per-op async task, holding the
provider's `std::Mutex` (no `await` under the lock, so no deadlock - it just
stalls that one worker while the first locked file on a volume snapshots).
`spawn_blocking` would be the textbook fix but complicates the COM apartment /
`Send` story; accepted as-is for V1 since a snapshot is created at most once per
volume per cycle. Revisit if the stall is observable. (The waits are now BOUNDED
- see M3.5 recheck2 P1-C below - so a hung writer can no longer stall it forever.)

## M3.5 recheck2 (round 2) - VSS robustness

### P1-C bounded VSS waits - DONE

`gather_async` now drives each `IVssAsync` with a finite `Wait(5s)` slice looped
to a 60s deadline (`VSS_S_ASYNC_PENDING` -> keep waiting; deadline blown ->
`VssError::Unavailable`, degrade to skip). `VssSnapshot::create` waits for the
worker's ready report with `recv_timeout(90s)`; on timeout it DETACHES the wedged
worker (does not `join`, which would re-block) and degrades. A detached worker
leaks one thread until process exit; the `VSS_CTX_BACKUP` shadow it may hold is
auto-released by the OS when the process dies. Accepted (a wedged VSS writer is
rare and the alternative is an unbounded hang).

### P1-A record-at-create - residual kill-9 window (benign)

A shadow's GUID is recorded into a per-orchestrator in-memory ledger SYNCHRONOUSLY
at create time (recorder hook), then flushed to the durable `vss.orphans` registry
by the per-source `record_vss_orphans`. The remaining window is a `kill -9` STRICTLY
between `VssSnapshot::create` returning and the enclosing source's
`record_vss_orphans` - the in-memory ledger is lost, so that GUID is not in the
durable registry. This is BENIGN: a `VSS_CTX_BACKUP` shadow is non-persistent and
the OS auto-releases it when the creating process dies, so a kill-9 orphan is
reclaimed by VSS itself; the registry is the belt-and-suspenders for the rare case
it is not, and the >1h startup sweep remains the backstop. A fully synchronous
durable record would need a blocking DB write from the sync hook (which runs on a
tokio worker, where `block_on` is unsound) or a second sync SQLite connection -
deferred as not worth the complexity for a benign window.

The DURABLE `vss.orphans` registry is process-global (one settings row, shared by
all account orchestrators); its read-modify-write is now serialized process-wide by
`orphan_registry_lock` (a `OnceLock<tokio::Mutex>`) so concurrent accounts cannot
clobber each other (P2-D). The create-LEDGER is per-orchestrator (not global) so
parallel accounts/tests never drain each other's pending records.

### P1-B VSS uploads non-resumable - DONE

A read served from a VSS snapshot (`read_path != live_path`) forces the simple
(non-resumable) upload path at every size: no resumable session is opened, no
`resume_identity` is stamped, so reconcile's resume-precedence block is never
entered for a VSS op and never reopens the live file. A failed VSS op preserves +
requeues cleanly (transient create -> `DeferToReconcile` keeps the op; hard failure
-> op dropped, next scan re-enqueues), so the next cycle re-snapshots + re-uploads
from scratch. Tested cross-OS via the `FakeVss` snapshot-dir + a cumulative
`resumable_sessions_opened` counter on the fake remote.

## M3.5 recheck-2 residual (cap reached)

The final M3.5 recheck (round 2) raised 1 P1 + 3 P2; three were FIXED (run_cycle applies
the current vss_mode to the provider before any VSS path; the provider map uses a checked
lookup instead of expect() so a concurrent end_cycle in the recorder gap degrades rather
than panics; a locked file under vss_mode=never is classified local.file_locked, not the
misleading local.vss_unavailable). ONE P2 is an accepted residual (cap-2 reached):

`cleanup_orphan_snapshots_once` sets `orphan_cleanup_done = true` before the registry
read/delete/write, and `read_vss_orphan_registry` swallows a DB read error as an empty
registry. So a TRANSIENT SQLite read error at startup skips orphan cleanup for the REST of
THIS process run. It is not a permanent leak: the flag is per-process (reset each start), so
the NEXT process retries the >1h sweep, and VSS_CTX_BACKUP shadows are OS-auto-released on
process death anyway. Proper fix (deferred): thread a Result through the registry read so
the done-flag is only set after a successful read + cleanup attempt, with a retry on a
transient DB error.

## M3.7 stress-harness documented V1 behaviours (tracked, not bugs)

The driven-chaos harness (M3.7) surfaces two genuine V1 behaviours as
ExpectedOutcome::DocumentedBehaviour scenario rows (executable, run every CI -
NOT #[ignore]'d, NOT weakened invariants). Tracked here for visibility:

- rename-storm churn: a rapid continuous rename storm + M3's once-per-boot
  reconcile (DESIGN s5.6) legitimately leaves transient stale `synced` rows for
  paths that were renamed AWAY (the file still exists under its new name, which
  IS backed up). It is NOT data loss and NOT a stuck pipeline - a subsequent
  full scan's delete-detection trashes the stale remote object. The harness's
  cross-scenario no-orphan/no-data-loss checks tolerate ONLY this renamed-away
  case (tolerate_rename_churn, scoped to the rename-storm row), still asserting
  no-duplicate-per-op-uuid + no-stuck-pipeline + that every still-existing file
  is backed up. Bounded transient churn, not an unbounded leak. A future
  improvement is more eager rename/delete reconciliation between scans.

- atomic-replace platform dependence: the SPEC s8 mid-upload replace defence
  surfaces local.file_replaced_during_upload where the platform exposes an
  inode/file-index, but local.file_changed_during_upload on Windows-stable
  (the file-index syscall is not on stable Rust, so fstat_identity reads inode
  0 and the size/ctime delta is the detecting signal). The replace-via-atomic-
  rename row injects a real upload window (with_slow_responses) + a monotonically
  growing body so the size delta is machine-speed-independent, and accepts
  either code - a documented platform-dependent outcome, not a faked pass.
