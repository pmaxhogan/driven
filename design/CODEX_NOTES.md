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

M4 status:
- **Windows metered detection is now REAL** (`crates/driven-power/src/network.rs`,
  `detect_metered` Windows arm). It instantiates the `NetworkListManager` COM
  object, queries `INetworkCostManager::GetCost`, and maps the
  `NLM_CONNECTION_COST` bitmask (FIXED / VARIABLE / CONGESTED / OVERDATALIMIT /
  APPROACHINGDATALIMIT / ROAMING -> metered; UNRESTRICTED / UNKNOWN / any COM
  failure -> the safe `false`). The `windows` crate features
  `Win32_Networking_NetworkListManager` + `Win32_System_Com` were added to
  `crates/driven-power/Cargo.toml` to back it.
- **macOS + Linux metered detection remain documented conservative defaults
  (`false`)** - ACCEPTED RESIDUALS. macOS `NWPath.isExpensive` needs a live
  `NWPathMonitor` on a dispatch queue (no one-shot synchronous read); Linux
  NetworkManager's per-connection `Metered` property is DBus-only internal NM
  state with no cheap synchronous `/sys`/`/proc` read, which does not fit the
  synchronous 30 s-cadence probe. Both keep `false` (safe direction: a false
  "not metered" only fails to skip a rare metered link; a false "metered" would
  stall ALL sync) until a monitor-backed / DBus-backed async reader lands.
- **Reachability** is still the coarse `true` hint in `PowerState`; the
  authoritative classification is owned by the network-resilience subsystem
  (DESIGN s5.8), which `driven-net`'s `ReqwestBackend` now implements for real
  (three-probe topology, hickory DNS re-resolution per probe).

Net effect: `skip_on_metered` is now LIVE on Windows; on macOS/Linux it stays
inert until those two arms are wired.

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

## M3.7 recheck rounds (codex) - closures + accepted residuals (cap reached)

Two codex recheck rounds ran on M3.7 (baseline 60d3a1c). Round 1 (7 findings):
finding 1 (disk-full-target) was resolved as an honest documented known-gap
(re-gated to `DiskMountAllowed`, a never-set env, because V1's read-only source
path cannot induce ENOSPC end to end - see the disk-full section's setup note);
F2-F7 were FIXED in commit 9abb3bd:
- F2 central s6.3 no-data-loss now enforces the FULL spec: object exists, md5
  matches drive_md5, and (unencrypted + retained-bytes) bytes hash to
  hash_blake3 (added blake3 dep + InMemoryRemoteStore::object_content).
- F3 drive-fileid-recycled asserts y.id == id_x; F4 distinct chaos-fake-drive
  gate; F5 fake trash/about use content_len(); F6 mutator-drive-daily-quota runs
  hermetically via with_daily_quota_after; F7 capability probes target the
  fixture-root volume.

Round 2 (FINAL, cap reached) raised 4 P1 + 2 P2. Two were FIXED; the rest are
ACCEPTED residuals (recheck cap reached, and none are regressions or affect the
green per-PR gates):

FIXED:
- fuzz `--duration` was silently capped at the 60s SCENARIO_WALL_CAP. `run_fuzz`
  now takes an explicit `wall_cap`: the registered `fuzz-smoke` stays bounded by
  a small step budget + 60s, while `fuzz --duration D` soaks by wall-clock for D
  (a local `fuzz --duration 6h` actually soaks 6h). Verified: `--duration 8s`
  ran 2038 steps over 8.6s.
- central duplicate-`client_op_uuid` check now counts over
  `list_folder_with_trashed` (including trashed), so "create two objects for one
  op, then trash one" is caught (matches the mutator checker). Safe: each upload
  op stamps a fresh uuid, so legitimate trash-then-recreate never collides.

ACCEPTED RESIDUALS (tracked, not faked-green):
- **Deferred-create pending-op exemption (reporting.rs assert_invariants).** The
  central s6.3 pending-op check exempts a due `upload` op carrying a
  client_op_uuid but no drive_file_id - the documented DESIGN s5.6 recovery
  handle a transient mid-first-upload fault leaves for the next-boot reconcile.
  Strictly, §6.3 calls any due row a leak; the exemption was added deliberately
  (M3 recheck) so the Drive-side transient + crash-recovery rows, whose CORRECT
  terminal state IS one such op, are not falsely red. The Drive-side fault rows
  ALSO route through the stricter `assert_invariants_opts` (byte-level +
  deferred-reconcile checks). Tightening this properly (persistent-fault ops
  should be backoff-scheduled, or scenarios should reboot+reconcile before
  asserting) is a cross-subsystem change best done with M4's pacer/backoff prod
  wiring; tracked as an M4 follow-up.
- **daily-quota midnight-resume not asserted (drive_side daily-quota-exhausted +
  mutator-drive-daily-quota).** Both rows assert the real
  `DriveDailyQuotaExhausted` code surfaces (hermetically, via the fake injector),
  but NOT the pacer's pause-until-midnight-Pacific + FakeClock resume - the chaos
  handle wires a NoopPacer. The scenario descriptions already state midnight-
  resume is M4. Injecting the real AIMD pacer + FakeClock control lands with M4's
  pacer prod wiring (the pacer is exercised by unit tests in driven-core today).
- **setup() is not wall-clock-capped (runner.rs).** The s6.3 no-infinite-loop cap
  wraps only run_assertions; setup is deliberately uncapped so the cacheable
  big-fixture builds (million-files-nested ~15 min, huge-file-*) are not killed.
  A truly-hung setup would hang until the outer CI job timeout. A future generous
  setup cap (well above the longest fixture build) that reports harness.timeout
  is a tracked robustness follow-up; no current scenario hangs setup.
- **huge-file fixtures not marked cacheable (file_size.rs).** huge-file-10gb /
  -50gb rebuild their 10/50 GB source every setup (no ctx.cacheable + versioned
  sentinel like million-files-nested). These rows are soak-gated (local
  `just chaos-soak` only, never per-PR), so the rebuild cost is bounded to the
  on-demand soak; adding the cacheable sentinel is a soak-efficiency follow-up.
- codex also flagged a "missing design/chaos-fuzz-smoke.json" - the ROADMAP M3.7
  acceptance does ask for a committed reference fuzz output. Added: a `--out PATH`
  flag on the `fuzz` CLI writes the full FuzzReport JSON (pass or fail), and
  design/chaos-fuzz-smoke.json is a real fixed-seed (0xDEADBEEF) 200-step passing
  run (no violation, all s6.3 invariants held).

## M3.7 CI cost policy (maintainer budget decision)

To bound GitHub Actions spend, the Chaos workflow deviates from the ROADMAP
"hermetic + fake-drive 3-OS PR-gating" acceptance by maintainer choice:
- chaos-hermetic + chaos-fake-drive run **windows-only** on every PR / main push
  (Windows is the primary target), and the **full 3-OS matrix only on `v*` tag
  pushes** (release gates) via a `startsWith(github.ref,'refs/tags/')` matrix
  switch. Unix-shaped rows run on the tag-push ubuntu leg + locally on Linux.
- The weekly 6h fuzz **soak cron is removed** from CI; the long fuzz + the
  soak-gated massive-input rows (million-files-nested, tiny-files-100k) run
  LOCALLY via `just chaos-soak` / `just chaos-fuzz`. The bounded `fuzz-smoke`
  row still runs in the per-PR sweep.
This is a deliberate cost/coverage trade, recorded here so the deviation from the
locked ROADMAP acceptance is explicit and intentional, not an oversight.

## M4 codex review - accepted residuals / deferrals

The M4 codex xhigh review (baseline 6099da5; file
`.claude/codex-reviews/M4-20260623-161005.md`) plus the in-workflow verify pass
produced 12 fixes (all landed this milestone) and 3 items that are NOT bugs in
M4 but assembly-gated seams or explicit V1 scope boundaries. The seams below are
BUILT and unit-tested in M4; M5 (the prod-shell assembly that wires the crypto
suite + keystore + orchestrator + executor + GoogleDriveStore into a running
binary) ACTIVATES them. Documented here honestly so the deferral is tracked, not
an oversight.

- **V-F - `needs_reauth` account-state transition (activated M5).** The refresh
  path classifies an `invalid_grant` as
  `DriveErrorClassification::AuthInvalidGrant` and the executor maps that to a
  fatal `auth.invalid_grant` op outcome (`AuthInvalidGrant`). What is NOT wired
  yet: nothing calls `mark_account_state(NeedsReauth)` / emits
  `account:needs_reauth`, because no production binary assembles
  orchestrator+executor+GoogleDriveStore (src-tauri is the M0 skeleton; the CLI
  bypasses the executor). The condition is fully SURFACED for the M5 shell to act
  on; M5 performs the account-state transition. The token_store.rs module + type
  docs were reworded this milestone to stop claiming the transition happens
  today (they now say "surfaced for the M5 shell to act on").
- **V-G - breaker-from-outcomes activation (activated M5).** The
  `BreakerReportingStore` decorator + `ExecutorDeps.network` seam are built and
  unit-tested (the `breaker_from_outcomes` test module drives a real
  `StdCircuitBreaker` through the decorator). The decorator is only inserted when
  the executor is constructed with `network = Some(probe)`, which today only the
  executor tests pass; the M5 prod executor assembly passes
  `network = Some(ReqwestBackend-backed probe)`. Wired-and-tested in M4,
  activated at M5.
- **C-P2-2 - Shared Drive destinations are V1 out-of-scope (V2).** Drive listing
  is `corpora=user` / `spaces=drive` and the store sets no
  `supportsAllDrives` / `includeItemsFromAllDrives` params. V1 targets the user's
  personal My Drive ("Drive-on-My-Drive", per the `remote_store.rs` `RemoteEntry`
  doc + DESIGN). Shared Drive destinations (threading a shared-drive id through
  list/create/update/trash/metadata/resumable) are a deliberate V2 feature, not
  an M4 bug. The `pagination.rs` `corpora=user` comment already notes the V1
  scope; this records it as an explicit, accepted boundary.

Note: the optional `hickory-resolver` DNS escalation (DESIGN s5.8.5 "custom
resolution if we discover OS resolver pathologies in the field") was dropped from
the dependency tree in M4 to clear RUSTSEC-2026-0119 (hickory-proto name
compression). The DNS probe now uses `tokio::net::lookup_host` per DESIGN s5.8.1
(which was always the specified primary path); hickory remains the documented V2
escalation option if a field need arises, to be re-added behind a feature then.

## M4 recheck-2 deferrals -> M5 (executor assembly)

The codex recheck-2 (FINAL round, recheck cap=2) raised three P1s. One was
reachable in M4 via `driven-cli` sync and is FIXED (R2-P1-2: `query_offset()` now
classifies 401/403/429/5xx exactly like `push_chunk` via the shared
`chunk_status_outcome` + `DriveError::from_response`, reserving
`ResumableSessionInvalid` for session-dead 400/404/410). The remaining two are
executor/state-layer work that is only REACHABLE once M5 wires the real
`GoogleDriveStore` into the production executor: in M4 the executor runs the
`InMemoryRemoteStore` fake and the CLI bypasses the executor's pending-op
machinery, so neither path is exercised by delivered M4 scope. They are accepted
residuals (recheck cap 2 reached, M4 is DONE - no recheck-3), tracked on the M5
task, NOT bugs in M4's delivered scope.

- **R2-P1-1 - durable corrupt-create cleanup (executor pending-op lifecycle).**
  `GoogleDriveStore::verify_md5_or_trash_create` (google/mod.rs) best-effort
  trashes its OWN corrupt create when the post-upload md5 verify fails. If that
  `trash()` call itself fails, the store returns only `DriveError::ChecksumMismatch`;
  the executor maps it to `UploadError::Failed` and DELETES the pending op. Result:
  a live corrupt Drive object is stranded with no reconcile handle, and the next
  scan can create a duplicate. The durable fix - persist the corrupt `file_id` in
  pending state, keep the pending op UNTIL the corrupt object is confirmed
  trashed, and retry the trash on the next cycle - is executor/state-layer work.
  Latent in M4 (the real store is not executor-wired; the CLI bypasses the
  executor), so it cannot occur in delivered M4 scope. It is also astronomically
  rare (requires Drive-side upload corruption AND a trash failure). Lands at M5.

- **R2-P1-3 - DESIGN s498-500 "3 consecutive checksum mismatches ->
  status='corrupt'" per-file counter is absent on the real-store path.** The
  per-file mismatch counter + `FileStateStatus::Corrupt` transition DESIGN s498-500
  requires is NOT present where a real-store checksum mismatch is handled: a
  mismatch maps to `UploadError::Failed`, deletes the pending op, and the
  orchestrator only defers scan timestamps + logs activity. There is no per-file
  persistent mismatch state. Implementing it needs per-file persistent mismatch
  state in the executor/state layer = M5 work. M4 corrected the now-honest comment
  at `crates/driven-core/src/executor.rs` (`DriveError::ChecksumMismatch`) to stop
  claiming the defence exists today; the counter itself lands at M5.

## M5 recheck rounds (codex) - recheck-3 ran (one-time exception), M5 CLOSED

THREE codex recheck rounds ran on M5. Recheck-1 + recheck-2 are summarised below;
recheck-3 was a ONE-TIME exception past the normal cap=2 because recheck-2's two
fixes (reconcile keep+retry, concurrent-ish shutdown) each introduced a follow-on
correctness gap. Recheck-3 raised 2 P1 + 1 P2, ALL FIXED in one push, and **M5
review is now CLOSED (no recheck-4):**

- **R3-P1-1 (FIXED) - shutdown drained accounts SERIALLY under one outer timeout
  -> could orphan a task.** `lib.rs::shutdown_orchestrators` ran each account's
  `AccountHandle::shutdown()` in a serial `for` loop wrapped in ONE outer
  `tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, ..)`. With two accounts (each run
  loop up to ~20s + each poller the 5s abort budget) the outer timeout could fire
  MID-drain; because `drain_or_abort` has already TAKEN the `JoinHandle` out of its
  slot, dropping that future detaches (orphans) the in-flight aborted task -
  violating the M5 no-orphans acceptance. Fixed by running ALL per-account
  `shutdown()` futures CONCURRENTLY via `futures::future::join_all` and REMOVING the
  outer timeout entirely (each `drain_or_abort` already self-bounds: await up to its
  budget, then `abort()` AND await the aborted handle). The now-unused
  `SHUTDOWN_DRAIN_TIMEOUT` constant was deleted. Test:
  `concurrent_shutdown_of_multiple_slow_accounts_leaves_no_orphans` (2 accounts,
  both run loops slow + forever pollers, paused virtual time; asserts every handle
  `is_finished()` and that the concurrent sweep completes inside ONE run-loop budget,
  which a serial drain could not). `shutdown_joins_every_per_account_task_no_orphans`
  + `run_loop_gets_full_drain_budget_not_the_short_bridge_timeout` still pass.

- **R3-P1-2 (FIXED) - reconcile update path retried a 404 FOREVER -> wedged the
  account.** recheck-2's R2-P1-1 made the update-`metadata` and create-`find_by_op_uuid`
  reconcile arms keep+retry (return `Err`) on ANY error. But a stale/missing Drive
  file id returns a DEFINITIVE 404 (real store: `DriveErrorClassification::Other` ->
  `DriveError::Other`; fake: a `"no object..."` message -> `DriveError::Other`), which
  then retried every cycle - and because `reconcile_once` runs BEFORE scan/execute,
  one stale update op stopped ALL backups for the account. Fixed by
  `reconcile_metadata_error_is_retryable(class)`: keep+retry (return `Err`) ONLY for
  transient / rate-limited / quota / `InvalidGrant` (auth still maps to needs_reauth
  via `to_reconcile_err`); for a definitive not-found / non-retryable error CLEAR the
  stale `file_state.drive_file_id` (new `StateRepo::clear_file_state_drive_file_id`,
  runtime `sqlx::query` so no `.sqlx` cache change) so the next scan re-creates the
  object as a fresh CREATE, and DROP the op so the account is NOT wedged (the create
  path has no recorded id to clear, so it just drops + continues). Tests:
  `reconcile_metadata_not_found_clears_stale_id_and_drops_op` (404 -> id cleared, op
  dropped, account proceeds), and the recheck-2 behaviour is PRESERVED -
  `reconcile_metadata_transient_error_keeps_the_pending_op` +
  `reconcile_invalid_grant_on_{create_lookup,update_metadata}_maps_to_needs_reauth`
  all still pass.

- **R3-P2-1 (FIXED) - Backoff still counted as an active sync cycle in
  notifications.** recheck-2 (R2-P2-2) remapped `Backoff` to NetworkAttention, but
  `tray::notify_for_state` still set `saw_active_cycle = true` for `Backoff`, so a
  startup that only ever hit Drive backoff before settling to `Idle` would fire a
  bogus "first sync complete" toast. Fixed by removing `Backoff` from the
  active-cycle group (it now behaves like a `Paused` blip: clears the error dedup
  latch only); `saw_active_cycle` is set ONLY by a real scan/plan/execute/verify
  (+ the gating `PowerCheck`). The firing decision was extracted into a PURE
  `decide_notify(&mut NotifyState, &OrchestratorState) -> NotifyOutcome` so it is
  unit-testable without an `AppHandle`. Test:
  `backoff_then_idle_does_not_fire_first_sync_complete` (also asserts a REAL cycle
  then `Idle` STILL fires exactly once - the feature is intact).

### Recheck-1 + recheck-2 (history)

Recheck-1 (zero-orphan-task shutdown P1 +
aggregate tray severity P2 + reconcile `invalid_grant` -> needs_reauth P2) were
all FIXED. Recheck-2 raised 1 P1 + 2 P2,
ALL FIXED in the same push:

- **R2-P1-1 (FIXED) - reconcile `invalid_grant` swallow + dangerous pending-op
  delete.** recheck-1 only mapped `invalid_grant` -> `ReconcileError::AuthInvalidGrant`
  for the corrupt-trash retry; the OTHER reconcile remote awaits (resumable
  resume, encrypted-parent `ensure_folder`, update `metadata`, create
  `find_by_op_uuid`, adopt) propagated plain `anyhow` Drive errors, so a revoked
  token during a NORMAL create/update reconcile was retried forever instead of
  marking `needs_reauth`. WORSE: the update-metadata branch's catch-all
  `_ => delete_pending_op` deleted the op on ANY metadata error (incl. auth /
  transient), losing the reconcile handle for an op that may have committed. Fixed
  by `to_reconcile_err` (maps `classify_drive_error == InvalidGrant` ->
  `ReconcileError::AuthInvalidGrant`) wrapping EVERY reconcile remote await, and by
  deleting the pending op ONLY on a SUCCESSFUL metadata/lookup result that PROVES
  the UUID absent/not-applicable - on error the op is KEPT (retry next cycle).
  Tests: `reconcile_invalid_grant_on_{create_lookup,update_metadata}_maps_to_needs_reauth`
  + `reconcile_metadata_transient_error_keeps_the_pending_op`.

- **R2-P2-1 (FIXED) - run loop drained with the short 5s bridge budget.** The run
  loop (the only task that can be mid-upload) shared the 5s `TASK_DRAIN_TIMEOUT`
  with the signal-only bridges, so a >5s in-flight upload was aborted on explicit
  Quit and the outer ~20s budget never applied to it. Fixed by giving the run loop
  its own `RUN_LOOP_DRAIN_TIMEOUT` (full ~20s) drained FIRST, then draining the
  auxiliary tasks with the short 5s budget; the lib.rs outer sweep guard is now
  derived from the run-loop budget + a margin. Test:
  `run_loop_gets_full_drain_budget_not_the_short_bridge_timeout` (paused virtual
  time). The zero-orphan guarantee is preserved (every handle aborted+awaited on
  timeout); `shutdown_joins_every_per_account_task_no_orphans` still passes.

- **R2-P2-2 (FIXED) - Backoff rendered the blue syncing icon despite aggregating
  as network-attention.** `state_severity` ranked `Backoff` as network-attention
  (rank 3) but `TrayIcon::for_state` / `tooltip_for` mapped it to Syncing, so
  `Backoff + Idle` showed a blue syncing icon instead of the DESIGN s8.1 yellow
  "Drive unreachable" attention state. Fixed by mapping `Backoff` ->
  `TrayIcon::NetworkAttention` + the service-down tooltip. Test:
  `backoff_is_network_attention`.

Accepted residuals (cap reached, M5 DONE - none are regressions):

- **Power suspend/resume seam (`apply_suspending` uncalled).** No
  `WM_POWERBROADCAST` hook is wired, so the suspend/resume apply path is present
  but not driven by an OS event. Pre-existing; deferred.
- **Flat-tile tray icons.** The generated tray tiles are solid-colour squares,
  not the final designed glyphs (`Syncing` is a static blue tile, not an animated
  spinner). Pre-existing; cosmetic.
- **Elevation live test gate-skips off-elevation.** The real-VSS elevation test
  honestly SKIPs when the runner is not elevated (CI lacks elevation). Pre-existing.

## M6 scaffold - hand-written typed IPC (deviation from SPEC s11 tauri-specta)

SPEC s11 specifies that the typed TS surface (DTO types + a `commands` wrapper)
is generated from Rust via `cargo xtask gen-ts` using **tauri-specta**. M6
deviates: the typed IPC surface is **HAND-WRITTEN**, not generated.

Why: tauri-specta requires `#[derive(specta::Type)]` (plus `specta::specta`
attrs) on every DTO + command, a `specta-typescript` exporter, an `xtask` crate,
and a CI gen-step that fails when the checked-in `.ts` drifts from Rust. For
Driven's small, slow-changing IPC surface that is more moving parts than it
buys. Instead:

- Backend DTOs (`src-tauri/src/commands/dtos.rs`) derive plain serde
  `Serialize`/`Deserialize` with `#[serde(rename_all = "camelCase")]` so they
  render camelCase over the wire.
- The frontend hand-writes matching `camelCase` interfaces in
  `ui/src/ipc/types.ts` and one typed `invoke` wrapper per command in
  `ui/src/ipc/commands.ts`, plus typed `listen` helpers in
  `ui/src/ipc/events.ts`.
- The pairing is kept in sync by convention + review (each TS interface cites
  its Rust counterpart). There is no codegen and no CI drift-check.

Caveat (the cost of the deviation): Rust<->TS shape drift is NOT caught
mechanically - a renamed/added field on a Rust DTO must be mirrored by hand. The
`ui/src/__tests__/ipc-commands.test.ts` test pins the command NAMES + argument
shapes (mocking the `@tauri-apps/api/core` `invoke` seam) so at least the call
contract is guarded; field-level drift relies on review.

NOTE the one M5 inconsistency the TS faithfully mirrors: the M5
`GlobalSyncStatus` / `AccountSyncStatus` DTOs (`src-tauri/src/commands/sync.rs`)
do NOT carry `#[serde(rename_all = "camelCase")]`, so they are snake_case on the
wire (`account_id`). The M6 DTOs are all camelCase. `ui/src/ipc/types.ts` keeps
`AccountSyncStatus` snake_case to match; do not "fix" it to camelCase without
also changing the Rust DTO.

Local folder/file picker = `tauri-plugin-dialog` v2 (added to `src-tauri`
Cargo.toml + registered in `lib.rs` after the notification plugin + `dialog:default`
in `capabilities/default.json`; `@tauri-apps/plugin-dialog` added to ui
package.json). `dunce` v1 added for the SPEC s11.6.1 `validate_writable_dest`
canonicalisation (Windows UNC-friendly). M7 `/activity` + M8 `/restore` are
PLACEHOLDER views in M6 (a t()-driven "coming later" shell); M6 implements
`/setup`, `/accounts`, `/sources`, `/rules`, `/about`.

## M6 recovery completion (settings.rs re-completed after a mid-run agent death)

The M6 implement phase ran three parallel agents. The backend-ipc agent fully
wrote `commands/{accounts.rs, sources.rs, mod.rs}` but died on a network blip
BEFORE writing `commands/settings.rs` (which the scaffold left as five `todo!()`
bodies), and the integrate pass never ran. This recovery filled the
`settings.rs` gap and ran the cut-off integrate. What landed:

- `settings.rs` - all five commands implemented FULLY (no `todo!()`/panic/fake):
  `get_settings`, `update_settings`, `export_diagnostic_bundle`,
  `check_for_updates`, `list_releases`. The anti-fake-green sweep shows ZERO
  non-test stub macros across the whole M6 command surface.
- New `src-tauri` deps (the scaffold had already shown M6 adds deps - dialog,
  dunce): `uuid` (the accounts wizard mints session ids - the backend-ipc agent
  used it but died before adding it to Cargo.toml, breaking the build),
  `reqwest` (workspace dep; the GitHub releases fetch), `semver` (version
  compare), `crc32fast` (the diagnostic-bundle ZIP CRC). No `zip` crate: the
  bundle is written by a small hand-rolled STORED-method ZIP encoder
  (`settings.rs` `ZipWriter`) to keep the dep + license surface minimal and
  `cargo deny` green.

### Settings KV: snake_case on disk, camelCase on the wire (storage bridge)

The migration 0002 seed writes each `settings` KV group in `snake_case`
(`auto_start_on_login`, ...), but the frozen M6 DTO groups
(`commands/dtos.rs`) are `camelCase` (`autoStartOnLogin`) per the M6 typed-IPC
convention above. Deserializing the seeded snake_case JSON directly into the
camelCase DTO FAILS (missing-field). So `settings.rs` keeps a `mod storage` of
`snake_case` structs that mirror the DTO groups field-for-field with `From`
conversions both ways: every settings READ deserializes a `storage::*`, every
WRITE serializes one, and the boundary converts to/from the DTO. The DB stays
canonical snake_case (one casing on disk, matching the migration); the wire
stays camelCase. `load_orchestrator_config` (shared with `sources.rs`'s
post-add reconfigure) reads the same storage structs.

### CommandError is camelCase on the wire (minor SPEC s24 example deviation)

`commands::CommandError` now derives `#[serde(rename_all = "camelCase")]`, so
it renders `retryAfterMs` rather than the SPEC s24 example's literal
`retry_after_ms`. This matches the M6 camelCase typed-IPC convention (and the
test the recovered `mod.rs` shipped). `code` + `message` are identical in both
casings and `details` is single-word, so only the retry-after hint differs; the
frontend reads only `.code`, so nothing downstream depends on the casing.

### Updater: M6 ships a REAL GitHub-releases backend; the Tauri manifest stays M9

ROADMAP M9 owns the `update.json` manifest hosting
(`driven.maxhogan.dev/updates`) + the `tauri-plugin-updater` download/relaunch
path; M9's sequencing note says the in-app updater "needs a real `update.json`
to fetch, which only exists once the release pipeline is in place" - that
endpoint does NOT exist in M6. So `check_for_updates` / `list_releases` do NOT
query that manifest. Instead they hit the GitHub releases API for
`pmaxhogan/driven` (real + reachable today): `list_releases(page)` returns the
channel-filtered page; `check_for_updates` returns `Some(UpdateInfo)` only when
the newest channel release's semver tag is strictly newer than the running
build. This is the honest "is there a newer release" answer the About tab needs.
The SIGNED-BUNDLE DOWNLOAD + INSTALL + RELAUNCH (the `tauri-plugin-updater`
glue, the `update.json` generation script, the `updater:downloaded` event) stay
M9 - they require the M9 ed25519 keypair + manifest hosting that do not exist
yet. No deferral-by-typed-error was needed: both M6 commands have real bodies.

### Two recovered-file bugs fixed during the cut-off integrate pass

- `Settings.vue`'s `parseOptionalPositiveInt`/`parsePositiveInt` assumed a
  `string`, but an `<input type="number">` bound via `v-model` yields a NUMBER,
  so `.trim()` threw at runtime - the settings-UI test caught it once the
  integrate pass finally ran them. Fixed to accept `string | number` and coerce.
- The `settings-components.test.ts` "Edit exclusions" test asserted
  `preview_exclusions` was called with a FLAT `{ localPath }`, but the frozen
  `previewExclusions` IPC wrapper nests the request under `{ req }` (matching the
  Rust `preview_exclusions(req: ExclusionPreviewRequest)` signature). The test
  assertion was corrected to the real `{ req: { localPath } }` contract (a
  contract fix, not a weakening).

## M6 codex review round-1 fixes (8 P1 + 3 P2)

Source review `.claude/codex-reviews/M6-20260624-011401.md` (baseline 3af8fc8,
M6 @ 80c2452): CI + Chaos were GREEN but the wizard/account/source/crypto
lifecycle had real end-to-end gaps the mocked unit tests did not exercise. All
11 findings fixed; new tests EXERCISE each fix (backend `#[cfg(test)]` +
`src-tauri/tests/ipc_path_validation.rs` + vitest the wizard now completes
end-to-end against the fake remote).

| Finding | What was broken | How it was fixed |
|---|---|---|
| P1-1 (B1) | Setup could not pick a Drive destination - `pick_drive_folder` echoed `current_folder_id: None` at root, so `setup.driveFolderId` was never set. | `pick_drive_folder` now resolves `None` -> the concrete Drive root alias `"root"` AND echoes it back as `current_folder_id`, so the user can select the current folder (incl. My Drive root). `add_source` accepts `"root"`. Test: `pick_drive_folder` root-listing mock + the wizard walk select the root id. |
| P1-2 (A2) | A newly added account had no running orchestrator until restart, so the wizard's initial `sync_now(sourceId)` failed "no running orchestrator". | `AppState.accounts` moved behind a sync `Mutex<HashMap<_, Arc<AccountHandle>>>` with `insert_account`/`remove_account_handle`; assembly's per-account build factored into `assembly::spawn_account(app, &AppState, id)`, called by `finish_add_account` to hot-spawn + insert the handle (mirroring the M5 no-orphan drain - a prior handle is shut down first). Tests: `dialog_token`/handle bookkeeping + the vitest wizard walk hits a running-orchestrator mock for `sync_now`. |
| P1-3 (A1) | BYO `client_id`/`client_secret` lived only in the in-memory wizard session; only the refresh token persisted, so after restart refresh fell back to env/default creds and FAILED for every BYO account (silent broken-account data loss). | New `ClientCredsStore` (keychain namespace `driven.google.client_creds`) persists the per-account client creds on `finish_add_account`; loaded everywhere a `RefreshingTokenSource` is built (`assembly::resolve_account_oauth_creds` used by boot `build_remote` + `pick_drive_folder` + reauth); deleted on `remove_account`. Secret never logged. Tests: `ClientCreds` encode/decode round-trip. |
| P1-4 (A3) | Reauth created a hidden session and expected `finish_add_account`, but the UI only received `authUrl` and never the session id, so reauth never completed. | `reauth_account` now returns `ReauthSession { sessionId, authUrl }` (seeded with the account's stored client creds, A1); the UI opens the URL, listens `oauth:complete`, then `completeReauth(sessionId)` -> `finish_add_account` re-stores the new token onto the EXISTING account (no duplicate) + flips it back to `ok` + hot-spawns it. Tests: accounts-store `reauth` + `completeReauth`. |
| P1-5 (B3) | The BIP39 recovery phrase was emitted as a transient event the UI never subscribed to; setup rendered the reveal BEFORE the source (empty phrase) and the confirm checkbox could be ticked with no phrase shown - so the app could create ENCRYPTED BACKUPS THE USER CAN NEVER RESTORE. | The phrase is now a ONE-TIME RETURN VALUE: `add_source` returns `AddSourceResult { source, recoveryPhrase }` (Some only when this opt-in generated the master key). `ensure_master_key` encodes the phrase BEFORE stamping the row and HARD-ERRORS (rolling back the key) if it cannot encode - never an unrestorable source. The UI shows the phrase via `RecoveryPhraseReveal` AFTER the source/key exists (setup confirm step; add-source a post-confirm reveal step) and gates Finish/Done on an explicit ack that is only enableable once a real phrase was displayed. Tests: store + vitest assert phrase returned, displayed, Finish disabled until acked. |
| P1-6 (B2) | The crypto provider snapshotted source rows at assembly; `reconfigure_account` only updated orchestrator config. So an encrypted source added/toggled while running failed CLOSED (no row -> Unavailable) until restart. | `KeystoreCryptoProvider.sources` moved behind a `Mutex` with `refresh(sources)` that swaps the live map AND invalidates cache entries whose crypto fields changed/vanished; the provider Arc is held on `AccountHandle.crypto`, and `reconfigure_account` reads the account's current rows and refreshes it after every source add/update/remove. Fail-closed preserved (missing key -> Unavailable, never plaintext). Tests: refresh picks up a new encrypted source (was unknown->Plaintext, now Unavailable), toggles invalidate cache, removal drops to Plaintext. |
| P1-7 (C1) | SPEC s11.6.1 requires dialog-derived paths; the impl took raw webview strings and fabricated a token from the untrusted parent. | The BACKEND now OWNS the dialogs: `pick_folder_dialog` / `pick_save_zip_dialog` (tauri-plugin-dialog Rust API via a oneshot) return `{ path, token }`; `AppState` holds a one-shot, TTL-bounded `token -> path` binding (`mint_dialog_token`/`take_dialog_token`). `add_source` takes `localPathToken` and `export_diagnostic_bundle` takes `token`; each resolves the path from the token (single-use) and REJECTS a path with no matching token, then runs `validate_writable_dest` (canonicalize / no-dotdot / no-symlink-leaf / confine-to-dialog-root / atomic). Frontend calls the backend dialogs. Tests: `src-tauri/tests/ipc_path_validation.rs` (traversal, symlink-at-leaf, non-existent parent, outside-root reject, valid) + `dialog_token` single-use/TTL. |
| P1-8 (C2) | About asked for a DIRECTORY and passed it as `dest`; the backend then renamed a temp ZIP over the directory path -> always failed. | `pick_save_zip_dialog` returns a concrete `.zip` FILE path (suggested name + zip filter); `export_diagnostic_bundle` resolves it from the token and `atomic_write`s the ZIP AT that file. Test: the path-validation IT writes + reads back a real archive at the confined dest; About uses `pickSaveZipDialog`. |
| P2-1 (C3) | The diagnostic bundle omitted `activity_last_30d.csv`, `logs/`, `crashes/`, and wrote "user_version not exposed". | Added `StateRepo::schema_version()` (real `PRAGMA user_version`); `build_diagnostic_zip` now adds `activity_last_30d.csv` (30-day activity, message+source hashed), `logs/` + `crashes/` from `<config>/driven/logs` through a redaction pipeline (`redact_log_text`: tokens -> `<token-redacted>`, paths -> `<path:hash>`, emails -> `<email:hash>`, drive-id-shaped -> `<fileid:hash>`), and the real `user_version`. Tests: schema summary has real `user_version`, activity CSV header + redacts message, redaction-pipeline unit tests. |
| P2-2 (A4) | The consent URL was opened twice (backend `start_oauth_signin` AND frontend). | The backend opener closure now ONLY captures the URL for the return value (no `open_system_browser`); the FRONTEND is the single owner that opens it (add-account + reauth). |
| P2-3 (A5) | Account email was a user label / `account-<id>`, not the Google email. | OAuth now requests the `userinfo.email`+`userinfo.profile` scopes; `finish_add_account` fetches `oauth2/v3/userinfo` (text + serde_json, no `json` reqwest feature) with the fresh access token and persists the real email + display name (fallback to a label on failure, never a fabricated address). Tests: userinfo parse (with + without name). |

### Playwright deferred-to-local (CI uses vitest for the wizard walk)

SPEC's end-to-end wizard coverage is exercised in CI by the vitest jsdom walk
(`setup-wizard.test.ts` drives welcome -> credentials -> source -> encryption ->
confirm against the fake backend, including the B3 phrase-gated Finish and the
C1 backend folder dialog). A real Playwright/WebDriver run against the built
Tauri app is deferred to a local pre-release check (no headless Tauri WebDriver
in the Windows-only PR gate); the vitest walk is the CI proxy.

## M6 codex recheck-1 fixes (round 2: 4 P1 + 4 P2)

The codex recheck-1 (baseline 3af8fc8, M6 @ 25b0b04) raised 4 P1 + 4 P2 - all
tightenings of the round-1 fixes (atomicity, fatal-not-best-effort, the
dialog-token rollout that missed `preview_exclusions`) plus a few untouched gaps.
This is the FINAL fix round (recheck cap = 2); all 8 are fixed below.

| Finding | What was broken | How it was fixed |
|---|---|---|
| R1-P1-1 (data-safety, `sources.rs`) | On the FIRST encrypted source, `ensure_master_key()` stored the keychain master key + stamped `accounts.encryption_master_key_id` BEFORE `upsert_source()`. A source-insert failure left the account "provisioned" but the user never got the phrase -> unrestorable encrypted backups, and a retry returned NO phrase. | The account-stamp + source-insert are now ATOMIC: new `StateRepo::insert_source_with_optional_master_key_stamp` does both in ONE sqlx transaction (sqlite override; a default impl covers test doubles). `add_source` splits master-key prep (`prepare_master_key`, which generates + stores the keychain key + encodes the phrase but does NOT stamp) from the atomic DB write; on a DB failure when a key was just generated it DELETES the keychain master key (`delete_master_key`) so the account is left unprovisioned and a retry re-reveals. Net invariant: either the command fully succeeds and returns the phrase, or it fully rolls back. Tests (`sqlite.rs`): forced FK-violation source insert rolls back the account stamp + leaves no orphan; retry succeeds; no-stamp path just inserts. |
| R1-P1-2 (`sources.rs`) | `preview_exclusions` walked a RAW webview `PathBuf`, so a compromised renderer could enumerate arbitrary readable directories (the round-1 token rollout covered `add_source` + export but MISSED preview). | The DTO now carries `local_path_token` (a backend-minted dialog token) XOR `source_id`. For a NEW candidate the path is resolved by a NON-CONSUMING `AppState::peek_dialog_token` (so the later single-use `add_source` TAKE still works as the user re-runs the preview); for an EXISTING source it is resolved from `backup_sources.local_path` by id. A request with neither / a bad token is rejected. Frontend: `AddSourceWizard.loadPreview` sends the token, `SourceTable.loadEditPreview` sends `sourceId`; `ipc/types.ts` updated. Tests: `peek_dialog_token` is non-consuming + TTL/single-use preserved; the SourceTable vitest asserts preview-by-`sourceId`. |
| R1-P1-3 (`sources.rs`) | `pick_drive_folder` always built a REAL `GoogleDriveStore`, ignoring `AppState::remote_mode()` - breaking the fake-remote wizard acceptance path and risking real-Google/keychain hits in fake/e2e runs. | Extracted `select_picker_store(remote_mode, account_id)`: `RemoteMode::Fake` builds an `InMemoryRemoteStore` + uses its synthetic root id (NO real store, NO keychain read); `RemoteMode::RealGoogleDrive` builds the live store + uses Drive's `"root"` alias. Test: `select_picker_store(Fake, random_id)` lists the fake root WITHOUT creds (a real-mode build would fail on the missing keychain entry). |
| R1-P1-4 (`accounts.rs`) | `store_client_creds` was best-effort, so a keychain-write failure still let `finish_add_account` succeed - leaving an account that refreshes with env/default creds after restart and FAILS (the refresh token is bound to the minting client). | `store_client_creds` now returns `CommandResult<()>` (FATAL). Fresh-add: token + creds are stored BEFORE the account row; a creds failure rolls back the just-stored refresh token and returns the error (no half-account). Reauth: creds persist (fatal) BEFORE the account is flipped to `ok`, so it stays needs_reauth on failure. No account may exist that cannot refresh its own token. |
| R1-P2-1 (`assembly.rs`) | Cold-start orchestrators always used `OrchestratorConfig::default()`, so persisted settings (scan cadence, bandwidth cap, metered/battery gates, VSS mode) only applied after a live edit. | `build_account` now reads `commands::settings::load_orchestrator_config(state)` at assembly time (the SAME loader `update_settings`/`reconfigure_account` use). Test (`assembly.rs`): a persisted non-default scan cadence + cap + gates are reflected in the cold-start config. |
| R1-P2-2 (`sources.rs`, DESIGN s5.2.2) | Overlapping / nested source roots were not rejected; `add_source` canonicalised the new path but never compared it to existing roots. | New `reject_overlapping_root` canonicalises every existing `backup_sources.local_path` and rejects (stable `local.io_error`) when the candidate is an ancestor of, descendant of, or identical to any existing root (applied GLOBALLY per DESIGN, which does not scope it per-account); siblings are allowed. Checked BEFORE master-key generation so an overlap never provisions a key. Test: nested + ancestor + identical rejected, sibling allowed. |
| R1-P2-3 (`stores/setup.ts`) | Leaving the encryption step always called `createFirstSource()`; going Back from confirm then Next again re-called it, but the one-shot folder token was already consumed -> the wizard wedged. | `createFirstSource` is now idempotent: it short-circuits when `sourceId` is already set (preserving the staged phrase + ack). Test: a second `createFirstSource` does NOT re-call `add_source` and does not error. |
| R1-P2-4 (`CredentialsWalkthrough.vue`, DESIGN s6.1) | The UI required a non-empty client secret, but the backend + DESIGN allow an empty secret for PKCE installed-app clients. | `canSubmit` now requires only a non-empty client ID; the (possibly empty) secret is passed through. Tests: submit allowed + the empty secret forwarded with a client ID; still blocked with no client ID. |

## M6 codex recheck-2 fixes (round 3: 4 P1 + 4 P2 - USER-GRANTED exception; M6 CLOSES after recheck-3)

The codex recheck-2 (baseline 3af8fc8, M6 @ 71efe9c) raised 4 P1 + 4 P2. Round 3
is a ONE-TIME exception past the normal recheck cap=2 (the user explicitly
approved it for these specific findings, analogous to M5's recheck-3). After this
push, codex RECHECK-3 runs and the M6 review CLOSES regardless - there is no
recheck-4. All 8 are fixed below. A new SPEC s24 code `internal.invalid_input`
(`ErrorCode::InvalidInput`, with its `en-US` i18n entry + the tray red-error
classification) was added for backend-side input-validation rejections (R2-P1-3 +
R2-P2-3).

| Finding | What was broken | How it was fixed |
|---|---|---|
| R2-P1-1 (data-safety, `sources.rs` + `sqlite.rs`) | Two concurrent `add_source` on an account whose `encryption_master_key_id` was still NULL could BOTH generate DIFFERENT master keys into the same keychain slot and wrap different source keys; SQLite then unconditionally stamped -> one source permanently unrestorable (its `wrapped_source_key` under a master key no longer in keychain). | BOTH defenses, per the spec: (1) a per-account async `tokio::Mutex` in `AppState` (`ensure_master_key_lock(account)`) held across the ENTIRE first-encrypted critical section (ensure-master-key -> stamp -> insert) - and the account master-key state is RE-READ inside the lock, so a losing-race second add observes the key the winner installed and wraps under the SAME key (`newly_generated=false`). (2) The SQL stamp is now a COMPARE-AND-SET: `UPDATE accounts SET encryption_master_key_id=? WHERE id=? AND encryption_master_key_id IS NULL`; on 0 rows it reads the current value and treats a same-key stamp as idempotent but a DIFFERENT-key stamp as a hard error (transaction rolled back, source NOT inserted) so a divergent key can never be committed. Tests: AppState lock is shared per-account / distinct across accounts / serialises a critical section; sqlite CAS rejects a divergent concurrent key (first key preserved, divergent source not persisted) and is idempotent for the same key (both sources persist under one key). |
| R2-P1-2 (regression from round-2, `sources.rs` + `assembly.rs` + `app_state.rs`) | The fake Drive picker and the fake orchestrator built DIFFERENT `InMemoryRemoteStore` instances, so a root folder id the picker minted was invisible to the uploader -> fake-mode setup made an unusable source. | `AppState` now holds a SHARED per-account fake-remote-store registry (`FakeRemoteStores = Arc<Mutex<HashMap<AccountId, InMemoryRemoteStore>>>`, get-or-create). `assembly::build_and_spawn` builds the registry BEFORE the account loop, threads it into `build_account`/`build_remote` (the orchestrator's fake store comes from it), then MOVES it into `AppState`; `spawn_account` (hot path) reads it from the running `AppState`; `select_picker_store` returns `AppState::fake_remote_store(account)`. `InMemoryRemoteStore` is `Clone` over a shared `Arc<Mutex>`, so every clone sees the same objects. Test: fake pick -> the uploader store creates a folder under the picker's root id -> the picker store lists it (round-trips the parent id in one shared store). |
| R2-P1-3 (`sources.rs` + `exclude.rs`) | include/exclude patterns were persisted with NO backend validation (only `preview_exclusions` validated, which callers can skip or patch around); an invalid/oversized glob then failed at scan-setup and stopped that source's backups. | New `driven_core::exclude::validate_patterns(include, exclude)` enforces max count per side (`MAX_PATTERNS_PER_SIDE`), max length per pattern (`MAX_PATTERN_LEN`), non-empty, and COMPILES each with the SAME `GitignoreBuilder` the scanner uses (`exclude` verbatim, `include` as its `!`-re-include form). Wired into BOTH `add_source` (request patterns, before any key gen) and `update_source` (the post-patch EFFECTIVE patterns) via `validate_source_patterns`, mapped to `internal.invalid_input`. Tests (`exclude.rs`): valid accepted; over-count, over-length, blank, and an uncompilable glob (trailing `\`) rejected on both sides. |
| R2-P1-4 (`settings.rs`, SPEC s18) | Diagnostic redaction was whitespace-token-based and only caught tokens that START with an absolute path, so `path=C:\Users\Pat Smith\Taxes\f.pdf`, quoted paths, paths with spaces, and UNC paths leaked user paths/filenames into the exported bundle. | Rewrote redaction as a `Redactor` (built once per bundle from the DB source roots + `USERPROFILE`/`HOME` + `USERNAME`). Per line: (1) EXACT case-insensitive substring scrub of known source roots (longest-first) + home dir + username (handles their spaces); (2) an ABSOLUTE-PATH-RUN scanner that detects Windows drive / UNC / Unix-abs starts at a left boundary and consumes embedded spaces when QUOTED (to the matching quote) or after `key=` (to the next `key=value` field), while a bare path stops at the first space (so trailing prose / an adjacent email is not swallowed); (3) the residual token scrub (OAuth tokens / emails / drive-ids). No new dep (hand-written scanner, not regex). Tests: `key=path with spaces`, quoted-with-spaces, UNC, and a configured source-root substring are all scrubbed; ordinary non-path text (incl. a lone `/`) is unchanged. |
| R2-P2-1 (`accounts.rs` + `assembly.rs`, BYO-only, SPEC s11.1 / DESIGN s6.1) | The backend shipped + fell back to a baked-in default Google client id, so a direct IPC call could start OAuth with no submitted creds. | Removed the `DEFAULT_CLIENT_ID` fallback from BOTH the wizard session (`resolve_creds` now returns `CommandResult` and REJECTS with `auth.consent_required` when no BYO id is submitted AND no env override is set; `start_oauth_signin` requires it before marking the session started; `finish_add_account` requires it too) and the assembly refresh path (`resolve_oauth_creds` is env-only now). The `DRIVEN_OAUTH_CLIENT_ID`/`_SECRET` env vars are KEPT solely as the test/e2e injection seam (the `google_e2e` suite lives in `driven-drive` and injects creds directly, so it is unaffected). Test: `resolve_creds` rejects when no creds, resolves to the submitted BYO creds otherwise. |
| R2-P2-2 (`accounts.rs`) | `finish_add_account` `take()`-consumed the session tokens before all persistence succeeded, and stored keychain creds before the account row; a DB insert failure made the session unreplayable and orphaned creds. | The session tokens are now READ by `clone` (not `take`) and the session is removed ONLY on full success, so a failed finish stays replayable. Fresh-add persistence is extracted into `persist_new_account` over an `AccountSecretStore` trait (real impl over the keychain): it stores token -> creds -> row, rolling back EVERY prior keychain write if a later step fails, so a forced row-insert failure leaves NO orphaned keychain entries. Tests: a forced row-insert failure rolls back both keychain entries (and returns the error); the happy path keeps both; a clone leaves the session's tokens intact (replayable). |
| R2-P2-3 (`settings.rs`, SPEC s22) | Settings IPC accepted unchecked numeric/enum values; a buggy/compromised renderer could persist zero/huge intervals, invalid log level/channel/locale/vss_mode, etc. | Added backend validators run BEFORE `store_group`: numeric ranges for scan interval, deep-verify interval, bandwidth cap (when set), concurrency override (1..=32, SPEC s22), and update-check interval; enum checks for `io_priority`, `log_level`, updater `channel`, `color_mode`, `tray_left_click_opens`, `vss_mode`; and a BCP-47-shape check for `locale`. Out-of-range / invalid -> `internal.invalid_input`. Tests: out-of-range numeric + invalid enum + malformed locale rejected, valid accepted. |
| R2-P2-4 (`settings.rs` + `state/mod.rs` + `sqlite.rs`, SPEC s18) | `schema.txt` only counted `accounts` + `backup_sources`. | New authoritative `KNOWN_STATE_TABLES` (every migration-defined table: accounts, backup_sources, file_state, file_state_fts, pending_ops, activity_log, settings, file_checksum_mismatch) + a `StateRepo::table_row_count(table)` method (allow-list guarded, since a table name cannot be a bound parameter). `build_schema_summary` now counts EVERY table. Tests: schema.txt contains a count line for every known table incl. file_state + pending_ops. |

Cross-cutting: backend/frontend contracts stayed in sync (the only UI change is the new `errors.internal.invalid_input` `en-US` locale entry; the DTO shapes are unchanged). The new sqlx `query!` (CAS SELECT) regenerated the workspace `.sqlx` offline cache (0 drift). All gates green: `cargo build/clippy(-D warnings)/test --workspace`, `build -p driven-app`, `deny check`, `fmt --check`; `pnpm lint/test:unit/build` (vue-tsc clean). Anti-fake-green stub sweep on the M6 non-test surface: zero `todo!`/`unimplemented!`/`unreachable!` (the planner/scanner `unimplemented!()` are pre-existing `#[cfg(test)]` FakeStateRepo doubles).

## M6 codex recheck-3 fixes (round 4: 3 P1 + 2 P2 - FINAL; M6 review CLOSES after recheck-4)

The codex recheck-3 (baseline 3af8fc8, M6 @ df46a0e) raised 3 P1 + 2 P2, all localized to the diagnostic redactor, the recovery-phrase reveal gate, and two scalar validators - mostly incomplete/regressed round-3 fixes. Round 4 is the FINAL M6 fix round (user-approved past the round-3 hard-stop). After this push, codex RECHECK-4 runs and the M6 review CLOSES regardless - whatever recheck-4 finds is documented as a residual; there is no round-5 / recheck-5. All 5 are fixed below.

| Finding | What was broken | How it was fixed |
|---|---|---|
| R3-P1-1 (`RecoveryPhraseReveal.vue` + `stores/setup.ts` + `SetupWizard.vue` + `AddSourceWizard.vue`) | `canFinish` gated only on `phraseAcknowledged`, so the confirm checkbox could be ticked while the phrase was still HIDDEN - a user could start encrypted backups they could never restore. | The reveal component now tracks `everRevealed` (latches true the first time the user reveals a present phrase), DISABLES the acknowledge checkbox until `everRevealed && hasPhrase` (with a "reveal first" hint via the new `recoveryPhrase.revealFirstHint` i18n key), and emits `update:revealed` so the parent gates Finish. The setup store adds `phraseRevealed` + `markPhraseRevealed(value)`; `canFinish = !hasRecoveryPhrase || (phraseRevealed && phraseAcknowledged)`; both are reset on a new phrase (`createFirstSource`) and on `reset()`, and `markPhraseRevealed(false)` force-clears the ack. `AddSourceWizard` mirrors this with a local `phraseRevealed` ref gating the reveal-step "Done" button. When the phrase prop changes the component re-locks (emits `revealed=false` + `confirmed=false`). Tests: dedicated `recovery-phrase-reveal.test.ts` (checkbox disabled until reveal; confirm only emitted after reveal+check; re-lock on phrase change; empty phrase never enables); setup-wizard store + walk tests assert acknowledge-without-reveal leaves Finish disabled, reveal+ack enables it, and a re-lock clears both. |
| R3-P1-2 (`settings.rs:redact_token`, SPEC s18) | Redaction only caught an OAuth token when the WHOLE whitespace token started with `ya29.` / `1//`, so `refresh_token=1//...`, `"access_token":"ya29...."`, and `file_id=<id>` leaked secrets into the shareable bundle. | `redact_token` now splits each whitespace token on VALUE separators (`= : " ' , { } [ ] ( ) < > ; & ?`) and redacts each VALUE segment via a new `redact_value` helper (email / `ya29.` access token / `1//` refresh token / long opaque drive-id), re-emitting the separators + key names verbatim so the `key=` structure survives for debugging. Tests: a key=value line and a JSON snippet both redact the embedded refresh/access tokens + file id while keeping the key names + an adjacent `op=upload` field. |
| R3-P1-3 (`settings.rs:replace_ci`, SPEC s18, no-panic-in-non-test) | `replace_ci` found match offsets in `haystack.to_lowercase()` then sliced the ORIGINAL `haystack` with them; a Unicode case fold that changes byte length yielded wrong spans -> mis-redaction or a PANIC on a non-char-boundary slice (a non-ASCII username/path during export). | `replace_ci` now walks the ORIGINAL char boundaries and, at each, attempts a case-insensitive char-by-char match (`ci_match_at`) of the (already-lowercased) needle - lowercasing each haystack char on the fly and handling multi-char case-fold expansion - so every returned span is a valid ORIGINAL-string byte range. Documented the caller contract (every call site passes a pre-lowercased needle; source roots are stored lowercased). Test (non-ASCII written as `\u{}` escapes to keep the source ASCII): a source root with an accented `e` (U+00E9) + sharp-s (U+00DF) redacts correctly and never panics; a dotted-capital-I (U+0130, a length-changing fold) input does not panic; an ASCII CI replace returns original-span slices. |
| R3-P2-1 (`exclude.rs:validate_patterns`, DESIGN 18.8) | Caps were 1000 include + 1000 exclude and 4096 BYTES per pattern, but DESIGN 18.8 caps TOTAL patterns at 256 and per-pattern length at 512 CHARS. | Replaced `MAX_PATTERNS_PER_SIDE=1000` with `MAX_PATTERNS_TOTAL=256` (the COMBINED include+exclude count) and `MAX_PATTERN_LEN=4096`->`512` measured in CHARS (`check_one_pattern` now counts `chars()`). Tests: exactly the total cap + exactly the length cap accepted; one past the combined total (split across both sides) and one past the char length rejected on both sides. |
| R3-P2-2 (`sources.rs:update_source`, DESIGN 18.8) | `update_source` accepted any `deep_verify_interval_secs` (0 = constant churn, `u32::MAX` = suppress for decades). | `settings.rs` exposes `DEEP_VERIFY_MIN`/`DEEP_VERIFY_MAX` (`pub(crate)`) and a `validate_deep_verify_interval(value)` helper sharing the SAME `check_range` bound the global settings validator uses (3600..=31_536_000), returning the stable `internal.invalid_input` s24 code; `update_source` calls it on the patch value BEFORE persisting. Test: 0, `u32::MAX`, just-below-min, and just-above-max rejected; the 7-day default + both inclusive bounds accepted. |

Cross-cutting: backend/frontend contracts stayed in sync (the only UI additions are the `recoveryPhrase.revealFirstHint` `en-US` key + the store's `phraseRevealed`/`markPhraseRevealed` surface; no DTO shape changed, no new sqlx query/migration). All gates green: `cargo build/clippy(-D warnings)/test --workspace` (539 passed, google_e2e + elevation honest gate-skip), `build -p driven-app`, `deny check`, `fmt --check`; `pnpm install` (lockfile unchanged), `pnpm lint/test:unit (43 passed)/build` (vue-tsc clean). Anti-fake-green stub sweep on the M6 non-test surface: zero `todo!`/`unimplemented!`/`unreachable!` (the planner/scanner/orchestrator `unimplemented!()` are pre-existing `#[cfg(test)]` Fake doubles, outside the touched surface).

## M6 codex recheck-4 (FINAL) - ACCEPTED RESIDUALS; M6 review CLOSED

The codex recheck-4 (baseline 3af8fc8, M6 @ f9fb164, CI + Chaos GREEN, vue-tsc clean)
raised **1 P1 + 6 P2** (`.claude/codex-reviews/M6-recheck4-20260624-095729.md`). Per the
user-set close point ("round-4, then truly close"), M6 review is now **CLOSED** - these are
ACCEPTED RESIDUALS tracked for the **M9 pre-GA hardening** pass (NOT faked-green; none are
normal-single-user-use data loss). R4-P1-1 is flagged DATA-SAFETY / top pre-GA priority.

- **R4-P1-1 [P1, DATA-SAFETY -> fix FIRST in M9] (`sources.rs:194`)** - the first encrypted
  source is written `enabled:true` and `reconfigure_account` runs BEFORE the user
  acknowledges the one-time recovery phrase. The reveal+ack gate (R3-P1-1) is CLIENT-SIDE
  only; if the app/renderer dies in the narrow window between `add_source` returning (phrase
  in hand) and the user ack, the source stays enabled + encrypted and future syncs create
  backups the user can never restore on a new machine. PROPER FIX (root-cause): persist the
  first encrypted source as DISABLED/pending; add a backend `ack_recovery_phrase_saved(source_id)`
  command that enables + reconfigures only after a durable ack; exclude pending sources from
  scheduler + manual sync. (Narrow crash-window, so deferred - but it is the correct design
  and supersedes the layered UI gates; do this before any real release.)
- R4-P2-1 (`sources.rs:432`) - `preview_exclusions` accepts BOTH `source_id` and
  `local_path_token` (silently prefers `source_id`) and builds the matcher WITHOUT
  `validate_source_patterns`. Fix: `match (source_id, local_path_token)` reject both/neither;
  validate patterns before building the synthetic source.
- R4-P2-2 (`sources.rs:401`) - `pick_drive_folder` always returns `current_folder_path=""`,
  so `backup_sources.drive_folder_path` is persisted blank/wrong. Fix: backend returns the
  real breadcrumb, OR frontend persists its own crumb instead of the empty backend value.
- R4-P2-3 (`sources.rs:194`) - `add_source` trusts renderer `display_name`/`drive_folder_id`/
  `drive_folder_path` into SQLite with no printable/path-shape/length validation. Fix: shared
  validators + length caps before building `SourceRow`.
- R4-P2-4 (`accounts.rs:109`) - OAuth wizard sessions live in a process-global HashMap with
  no TTL/cleanup except a successful finish; abandoned flows accumulate. Fix: add
  created/updated timestamps, expire stale/terminal sessions, expose a cancel command.
- R4-P2-5 (`accounts.rs:696`, `settings.rs:1777`) - the userinfo + GitHub-releases
  `reqwest::Client`s have no connect/total timeout; a blackholed request hangs the IPC
  command. Fix: explicit `connect_timeout` + `timeout`.
- R4-P2-6 (`sources.rs:742`) - overlap detection SKIPS an existing source whose path no
  longer canonicalizes (fail-open), letting a temporarily-missing root be overlapped then
  revived into a nested-source state. Fix: persist canonical roots at add time and compare
  against the stored value, or fail closed when an existing root cannot be resolved.

These (esp. R4-P1-1) are tracked as a first-class pre-GA task; M9 (release pipeline) is the
gate before any real release, so the hardening pass lands there.

## M7 - Activity dashboard (ROADMAP M7)

M7 builds the Activity dashboard end-to-end. No spec deviations; a few design
decisions worth recording for later milestones:

- **Pagination is OFFSET-based, not a cursor.** SPEC s11.4 / s18.8 define the
  page selector as `PageRequest { page, limit }` (offset = page*limit) and the
  existing `StateRepo::query_activity` (shipped pre-M7) already implements it
  that way with a `1..=10_000` limit guard and a matching `COUNT(*)` total.
  M7 REUSES that method verbatim rather than introducing a `(timestamp, id)`
  cursor. The task brief preferred a cursor "so live inserts do not shift
  pages"; that risk is handled CLIENT-SIDE instead: the live tail and the paged
  history converge into ONE list in the Pinia store, deduped by row id, so a row
  that a new insert shifts across a page boundary is recognised as already-seen
  and never double-counted. Net effect matches the cursor goal (scroll back
  through 1000+ events with no duplicates and no re-query of earlier pages)
  without a schema/SQL change or sqlx re-prepare (0 drift). A true keyset cursor
  is a possible later optimisation if offset depth becomes a perf concern.
- **`activity:new` emission is now GUARANTEED at the single orchestrator
  chokepoint.** The event channel + `emit_activity_new` helper existed since M5
  (`#[allow(dead_code)]`, uncalled). M7 adds `OrchestratorEvent::ActivityWritten
  { entry: ActivityEntry }` and a private `Orchestrator::record_activity(row)`
  helper that writes the row AND broadcasts the assigned-id entry; all four
  orchestrator activity-record helpers (`record_collisions`,
  `record_ads_skipped`, `record_invalid_filenames`, `record_outcome_activity`)
  now route through it, so EVERY durable activity row emits. The app-shell event
  bridge (`assembly.rs spawn_event_bridge`) translates `ActivityWritten` ->
  `activity:new`. In NON-test code the orchestrator is the SOLE `write_activity`
  caller (the only other call site is a settings.rs test), so this covers the
  whole production write path.
- **`ActivityEntry` is the single wire DTO** (defined in `driven-core` types,
  serde camelCase, `From<&ActivityRow>`): it is both the `activity:new` payload
  AND the per-row element of `ActivityPageDto`, so the live tail and the paged
  history share one shape with no drift. `query_activity` IPC bounds the page
  `limit` to `1..=1000` and validates `min_level` (enum), `source_id` (UUID),
  and the `event_types` IN-list (count + per-entry length) before the query
  (SPEC s11.6.1; scalar-only filters, no raw paths). The retention command
  `clear_activity_older_than` passes through to the batched prune with a
  5M-row hard cap.
- **"Export diagnostic bundle" button** (ROADMAP M7 task list; DESIGN s8.3
  places it on the Activity dashboard). The backend command
  (`export_diagnostic_bundle`) + its backend-owned save-dialog/dialog-token flow
  shipped in M6 and is also surfaced on the About tab. M7 adds the button to
  Activity.vue per DESIGN s8.3, reusing the SAME `pickSaveZipDialog` ->
  `exportDiagnosticBundle` flow (no new backend work); both entry points call the
  one M6 command.

All gates green: `cargo build/clippy(-D warnings)/test --workspace` (driven-core
185 + driven-app 94 + the rest; google_e2e + elevation honest gate-skip),
`build -p driven-app`, `deny check`, `fmt --check`; `pnpm install` (lockfile
unchanged), `pnpm lint/test:unit (54 passed, 11 new activity-store)/build`
(vue-tsc clean). Anti-fake-green stub sweep on the touched surface
(`src-tauri/src` + orchestrator.rs/types.rs): zero `todo!`/`unimplemented!`/
`unreachable!` in non-test code (the orchestrator `unimplemented!()` are
pre-existing `#[cfg(test)]` Fake doubles).
