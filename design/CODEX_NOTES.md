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
