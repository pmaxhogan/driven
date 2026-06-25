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

## M7 codex review round-1 fixes (1 P1 + 6 P2)

The codex round-1 review (`.claude/codex-reviews/M7-20260624-103442.md`, baseline
f9fb164, M7 @ 9771d53; CI + Chaos GREEN on 3 OS, vue-tsc + eslint clean) raised 1
P1 + 6 P2 - all verified legitimate and all fixed. No spec deviations; the fixes
are additive (two new IPC commands + one new event + store hardening).

- **M7-P1-1 (live tail drops events on broadcast lag).** The per-account
  `OrchestratorEvent` broadcast is bounded (cap 256); the event bridge previously
  only LOGGED `RecvError::Lagged`, so an error storm permanently dropped
  `activity:new` rows from the live tail (violates DESIGN s8.3 last-1000 + ROADMAP
  M7 <500ms). Fix: on lag the bridge emits a new typed `activity:lagged` gap
  signal (SPEC s11.7, `events::emit_activity_lagged`); the webview store
  RECONCILES by re-querying `query_activity` page 0 for the current filter and
  dedup-merging the rows into the live tail (the durable `activity_log` is the
  source of truth), so no durable row is lost. The 500ms-typical path stays
  event-driven via `activity:new`. The bridge's per-event decision was factored
  into a pure `classify_bridge_event -> BridgeAction` so the Lagged->reconcile
  mapping is unit-testable WITHOUT a Tauri `AppHandle` (3 new assembly tests);
  the store side has a lag-reconcile merge test.
- **M7-P2-1 (stale-response race).** `loadInitial`/`loadMore`/`applyFilter` had no
  generation guard. Fix: a `requestToken` (bumped per load) + a filter snapshot;
  a response commits ONLY if the token + filter still match (`sameFilter`). New
  store test: a filter change mid-flight discards the stale response.
- **M7-P2-2 (unbounded live tail).** Live events grew `entries`/`seenIds` forever.
  Fix: the live tail is now a SEPARATE `liveEntries` list capped to
  `LIVE_TAIL_CAP` (1000, DESIGN s8.3) by evicting the oldest live entry on
  overflow; the rendered `entries` is `liveEntries` (deduped) ++ paged
  `historyEntries`, so an error storm is bounded while LOADED history pages are
  preserved. Two new store tests (cap holds; history not evicted).
- **M7-P2-3 (subscribe-before-unmount listener leak).** `subscribeLive` now tracks
  a `desiredSubscribed` flag; if `unsubscribeLive` runs before `listen()`
  resolves, the resolved unlisten fns are invoked immediately on arrival. New
  store test drives the unsubscribe-before-resolve race.
- **M7-P2-4 (event-type filter unreachable for history).** The dropdown was
  derived only from loaded rows. Fix: new backend `distinct_activity_event_types`
  IPC + `StateRepo::distinct_activity_event_types` (`SELECT DISTINCT ... ORDER
  BY`); the store loads it into `eventTypeOptions` and the view binds the dropdown
  to it. New backend repo test (sorted-unique set).
- **M7-P2-5 (missing DESIGN s8.3 header aggregates).** New backend
  `activity_summary` IPC + `StateRepo::activity_summary` returning bytes uploaded
  today / this week (summed `activity_log.bytes` over caller-supplied LOCAL day /
  week boundaries - so "today" honours the user's timezone with NO backend
  timezone crate), file count by `file_state.status`, and a recent-throughput
  window (bytes + window-ms, the UI derives bytes/sec). The view renders the
  header; bytes/rate via `Intl.NumberFormat` (DESIGN s8.7). New backend repo test
  (boundary-correct sums + status grouping) + the `file_state_status_str` mapping
  test.
- **M7-P2-6 (errors rendered via `String(e)`).** Activity load + diagnostic-export
  errors now normalize to the stable `{ code }` shape (SPEC s24) via a new shared
  `ui/src/ipc/errors.ts#toErrorCode` (promoted from `stores/setup.ts`, which now
  imports it) and render via `t(\`errors.${code}.long\`)` (the M6 pattern). The
  store exposes `errorCode` (was `error`); the view localizes it. New store test:
  a Tauri object error surfaces its code.

Cross-cutting: backend/frontend contracts stayed in sync (new DTOs
`ActivitySummaryDto`/`FileStatusCountDto` in both `dtos.rs` + `ipc/types.ts`; new
command wrappers + the `onActivityLagged` event helper). New i18n keys:
`activity.summary.*`, `activity.status.*` (en-US). Two new `sqlx::query!`
(DISTINCT + the summary aggregate) regenerated the `.sqlx` offline cache (0
drift). The two new `StateRepo` methods have trait DEFAULT impls (empty / zeroed)
so the `#[cfg(test)]` Fake doubles compile unchanged; the SQLite repo overrides
both with the real SQL. All gates green: `cargo build/clippy(-D warnings)/test
--workspace` (driven-core 189 incl. 2 new + driven-app 98 incl. 4 new; e2e_fake
20 pass/5 gate-skip), `build -p driven-app`, `deny check`, `fmt --check`; `pnpm
install` (lockfile unchanged), `pnpm lint/test:unit (62 passed, 8 new)/build`
(vue-tsc clean). Anti-fake-green stub sweep on the touched surface: zero
`todo!`/`unimplemented!`/`unreachable!` in non-test code.

### Residual / not-fixed
None. All 1 P1 + 6 P2 are fully fixed with exercising tests. One acceptable known
limitation (not a review finding): the M7-P2-3 fix covers unsubscribe-before-
resolve; a pathological re-subscribe DURING a not-yet-resolved subscribe could
orphan the second listener set, but that path does not occur in the Activity
view's mount/unmount lifecycle (V1 scope).

## M7 codex recheck-1 fixes (round 2: 2 P1 + 4 P2)

The codex recheck-1 (`.claude/codex-reviews/M7-recheck1-20260624-111403.md`,
baseline f9fb164, M7 @ 2e32870; CI + Chaos GREEN on 3 OS) raised 2 P1 + 4 P2 -
all verified legitimate and all fixed. This is the final fix round (recheck cap =
2); codex recheck-2 runs next and M7 closes after it. No spec deviations; the
core fix (R1-P1-1) is what makes the dashboard non-empty on the happy path.

- **R1-P1-1 (successful uploads/trashes recorded NO activity row).** The happy
  path was silently invisible: `record_outcome_activity` did `OpOutcome::Done {
  .. } => continue`, and the executor committed successful uploads without writing
  any `activity_log` row. So a healthy backup showed "No activity yet", emitted no
  live `activity:new`, and every DESIGN s8.3 byte aggregate ("Uploaded today /
  this week" + throughput) was zero - the dashboard's whole purpose was broken.
  Fix: `OpOutcome::Done` now carries `kind: DoneKind { Upload | Trash }` + `bytes:
  Option<u64>` (set at the two executor commit sites from the verified `post.size`
  for uploads, `None` for trashes). `record_outcome_activity` records an Info
  `upload_done` row WITH its byte count and an Info `trash_done` row (SPEC s24
  schema vocabulary), routed through the SAME `record_activity` chokepoint so
  `activity:new` broadcasts for success too and the existing prune / row-cap
  retention applies unchanged (ordinary `activity_log` rows). New orchestrator
  test: a successful upload writes + broadcasts an `upload_done` row carrying its
  bytes (and a `trash_done` row); the existing sqlite `activity_summary` test
  proves those bytes feed the header aggregates.
- **R1-P1-2 (lag reconcile only covered page 0).** `activity:lagged` recovery
  re-queried only page 0 (100 rows), but the live-tail contract is the last 1000
  and the broadcast buffer is 256, so a burst > 100 permanently left recent
  durable rows out of the visible tail until manual pagination; `events.ts` also
  discarded the `skipped` count. Fix: the `activity:lagged` payload is now typed
  (`ActivityLaggedPayload { skipped }`) and threaded through; the store's
  `reconcileFromHistory(skipped)` pages FORWARD covering `max(page, skipped +
  page)` rows capped at `LIVE_TAIL_CAP` (1000), collecting new (deduped) rows
  newest-first across pages and pushing them oldest-first so global order is
  preserved, stopping early once a page yields nothing new or history is
  exhausted. New store test: a lag with `skipped` > page size recovers all 250
  missing rows into the tail, in order, no dups.
- **R1-P2-1 (header aggregates went stale during active backup).** The summary
  loaded once on mount and never refreshed on live activity. Fix: a byte-carrying
  live event (an upload) schedules a debounced `loadSummary()`
  (`SUMMARY_REFRESH_DEBOUNCE_MS` = 750, so an upload burst fires ONE trailing
  reload), and a lag reconcile that recovered rows also refreshes the summary; the
  debounce timer is cleared on `unsubscribeLive`. Two new store tests: a
  byte-carrying live event triggers exactly one debounced reload; a non-byte event
  does not.
- **R1-P2-2 (throughput undercounted at week boundary).** `activity_summary`
  gated all three sums with an outer `WHERE ts >= week_start`, so near the start
  of a week any throughput-window row before `week_start` was dropped before the
  per-sum CASE ran. Fix: outer filter is now `WHERE ts >= MIN(?1, ?2, ?3)` (day /
  week / throughput starts) so each CASE owns its own window. Regenerated the
  `.sqlx` offline cache (one query hash changed, 0 drift). New sqlite test: a row
  inside the throughput window but before `week_start` is counted in throughput
  (and correctly excluded from today/week).
- **R1-P2-3 (raw event-type codes rendered).** The table showed the backend
  `eventType` verbatim. Fix: a shared pure `activityEventLabel(eventType, t, te)`
  helper localizes via `activity.events.<eventType>`, falling back to
  `errors.<eventType>.short` (error/skip codes already localized), then to the raw
  code as a SAFE fallback (forward-compatible / unknown types never blank or
  throw); the cell keeps the raw code as a `title` tooltip. New `activity.events.*`
  i18n keys for every curated type incl. the new `upload_done` / `trash_done`. New
  unit test exercises all three branches of the lookup chain.
- **R1-P2-4 (blank line at EOF).** Removed the trailing blank line in
  `stores/setup.ts` (`git diff --check` clean).

Cross-cutting: backend/frontend contracts stayed in sync (`OpOutcome::Done` +
`DoneKind` <-> the activity event vocabulary; typed `activity:lagged` payload in
`events.ts`). All gates green: `cargo build/clippy(-D warnings)/test --workspace`
(driven-core incl. the new upload-activity + throughput-boundary tests; e2e_fake +
elevation honest gate-skip), `build -p driven-app`, `deny check`, `fmt --check`,
`git diff --check`; `pnpm lint/test:unit (8 new across activity-store +
activity-event-label)/build` (vue-tsc clean). Anti-fake-green stub sweep on the
touched surface: zero `todo!`/`unimplemented!`/`unreachable!` in non-test code.

### Residual / not-fixed (recheck-1)
None. All 2 P1 + 4 P2 are fully fixed with exercising tests. The R1-P2-1 debounce
(750ms) means the header aggregate can lag a live upload by up to that window -
intentional (coalesces a burst into one query); the live tail itself is still
sub-500ms via `activity:new`.

## M7 codex recheck-2 fixes (round 3: 2 P1 + 4 P2, USER-APPROVED past the cap-2)

The codex recheck-2 (`.claude/codex-reviews/M7-recheck2-20260624-140630.md`,
baseline f9fb164, M7 @ 1da5b59; CI + Chaos GREEN on 3 OS) raised 2 P1 + 4 P2 -
all activity-dashboard correctness/validation (not data-safety). The user
explicitly approved a round-3 to fix ALL SIX (past the normal cap-2, analogous to
the M6 exception); codex recheck-3 runs next and M7 closes after it. No spec
deviations. Fix spec: `.claude/m7-codex-fix-spec-r3.md`.

- **R2-P1-2 (offset pagination over a live-prepended table) -> KEYSET.** The two
  P1s share one root: `query_activity` was OFFSET-based over `activity_log`, which
  is actively PREPENDED to, so rows inserted between `loadInitial` and `loadMore`
  shifted every later page (skip/underload while still advancing). Fix: switched
  `query_activity` to KEYSET pagination. `state::PageRequest` is now a `(before_ts,
  before_id)` cursor + `limit` (with `::first(limit)` / `::after_cursor(ts,id,
  limit)` ctors); the SQL pages `WHERE ... AND (?6 IS NULL OR ts < ?6 OR (ts = ?6
  AND id < ?7)) ORDER BY ts DESC, id DESC LIMIT n` so ties on `ts` are stable.
  `ActivityPage` gained `has_more` (a full page MAY have more older rows). The DTOs
  (`PageRequestDto` -> `beforeTs`/`beforeId`, `ActivityPageDto` -> `nextBeforeTs`/
  `nextBeforeId`/`hasMore`, dropped `page`) + `ipc/types.ts` + the store + the
  Activity.vue caller were updated in the same pass. The store carries the oldest
  loaded `(ts,id)` as `oldestCursor` and pages by it. One sqlx re-prepare (0 drift:
  one offset query removed, one keyset query added). Tests: `sqlite.rs`
  `query_activity_keyset_is_stable_under_inserts` (newer prepended rows never shift
  an older keyset page) + the rewritten `..._paginates_correctly` (cursor walk);
  store `loadMore pages by the oldest (ts,id) CURSOR`.

- **R2-P1-1 (lag reconcile early-stops before the gap is covered).** The reconcile
  broke on `recoveredThisPage === 0`, which could stop at an already-seen newest
  page while the dropped rows sat DEEPER (a ring-buffer broadcast evicts the OLDEST
  of a burst, so dropped rows are below the latest delivered). Fix: the reconcile
  now walks by the keyset cursor over a bounded SCAN BUDGET (`min(LIVE_TAIL_CAP,
  max(PAGE_SIZE, skipped + PAGE_SIZE))` rows) and does NOT stop on a zero-new page;
  it stops only on history-exhausted or budget-spent. Recovered rows can be older
  than rows already in the tail, so they are merged in SORTED (newest-first) order
  via the new `mergeRecoveredLive` (not blindly prepended), then capped. Tests:
  store `recovers DEEPER dropped rows even when the newest page is already seen`
  (the exact recheck-2 P1 shape) + the existing multi-page recovery test.

- **R2-P2-1 (per-op activity lost on a mid-plan crash).** Successful upload/trash
  activity was written in a POST-PASS after `executor.execute()` finished the whole
  source plan, so a crash/shutdown mid-plan lost the audit rows + byte aggregates,
  and a large initial backup showed no per-file activity until completion. Fix:
  streamed per-op activity. `Executor::execute` takes a new `on_outcome:
  &OutcomeSink<'_>` (a boxed-future per-op sink); the DefaultExecutor invokes it
  INSIDE `ExecOne::run` right after the op's durable commit (and BEFORE returning),
  so the activity DB write runs as part of the in-flight future polled by the
  FuturesUnordered drain loop. (Doing it in the drain loop's select arm instead
  deadlocked the single-connection pool against a concurrent op holding the
  connection - found via 4 chaos tests timing out at 120s; the in-`run` placement
  fixes it.) The orchestrator's `on_outcome` builds the `NewActivity` synchronously
  from the borrowed outcome and the future borrows only `self` (satisfies the sink
  bound). Routes through `record_activity` so `activity:new` still broadcasts per
  op. `noop_outcome_sink` is exposed for the chaos harness + tests. Test:
  `orchestrator.rs` `per_op_activity_survives_a_mid_plan_stop` (RecordingExecutor
  streams N outcomes then errors; the committed op's row persists).

- **R2-P2-2 (unbounded `before_ts` wipes the log).** `clear_activity_older_than`
  accepted any `before_ts` (an `i64::MAX` prunes to the hard cap). Fix: a shared
  `validate_timestamp_bound` (>= 0 and <= now + 1 day, stable
  `internal.invalid_input`) gates `clear_activity_older_than` AND the
  `query_activity` `sinceMs`/`beforeMs` filters AND the keyset `beforeTs`. Tests:
  `activity.rs` `validate_timestamp_bound_rejects_out_of_range`,
  `validate_filter_rejects_out_of_range_time_filters`,
  `validate_page_rejects_out_of_range_before_ts`.

- **R2-P2-3 (silent u64<->i64 wrap on counts).** `activity_log.file_count` /
  `bytes` were cast `u64 -> i64` and back with `as`, so a value > `i64::MAX` wrapped
  negative and summaries clamped to 0. Fix: `write_activity` uses `i64::try_from`
  and REJECTS an over-range value (`internal.bad_request`); the read path decodes
  via a new `decode_nonneg_u64` (`u64::try_from`, rejects a negative stored value).
  Test: `sqlite.rs` `write_activity_rejects_counts_above_i64_max` (over-cap
  rejected, `i64::MAX` boundary round-trips).

- **R2-P2-4 (raw event codes in the filter dropdown).** The event-type filter
  rendered raw backend codes (`{{ et }}`) while the table localized them. Fix: the
  dropdown option text now uses the shared `activityEventLabel` helper (via
  Activity.vue's `eventLabel(et)`), keeping the raw code as the option value +
  `title`. Test: `activity-filter-dropdown.test.ts` (mounts Activity.vue, asserts a
  known code renders its localized label while value/title stay the raw code, and
  an unknown code falls back to the raw code).

### Residual / not-fixed (recheck-2)
None. All 2 P1 + 4 P2 are fully fixed with exercising tests. Keyset `has_more` is a
conservative "a full page MAY have more" (an exactly-full final page costs one
extra empty fetch, never a skipped row). The per-op activity sink runs one write
per op (correctness: no mid-plan audit loss + per-file visibility); DESIGN s18.4's
1-second batched activity writer remains a future optimization, not required for
the correctness fix.

## M8 - Restore browser (ROADMAP M8)

Restore flow (SPEC s11.5, DESIGN s8.4): browse what is backed up, search by
filename/glob, and restore selected files to a local folder - INCLUDING the
encrypted path (decrypt filename for display + STREAM-decrypt content to disk).

### What landed

- **State layer (driven-core).** Two new `StateRepo` methods, each with a default
  impl (so the test fakes need no per-fake stub) overridden by `SqliteStateRepo`:
  - `list_file_state_under_prefix(source, prefix, limit)` -> `Vec<RestoreFileRow>`:
    an INDEXED range scan over the `(source_id, relative_path)` PK
    (`relative_path = prefix OR (>= 'prefix/' AND < 'prefix0')`), so a folder open
    is one range query, not a Drive call (ROADMAP M8 navigation reads file_state).
    Returns `limit + 1` rows so the caller detects truncation without a COUNT.
  - `search_files_glob(source, pattern, limit)`: a `relative_path GLOB ?` scan for
    wildcard queries the FTS5 tokenizer cannot express (`*.rs`, `src/*`). The
    existing FTS5 `search_files` keeps serving prefix/term queries.
  - New `RestoreFileRow` row type. Tests: subtree range scan (incl. the
    `prefix0` upper bound excluding a sibling like `srcfoo.txt`), limit+1
    truncation, glob wildcard match. sqlx-prepare ran; 0 drift.

- **Restore IPC (src-tauri `commands/restore.rs`).** Four real `#[tauri::command]`s:
  - `list_remote_tree(source_id, prefix)`: derives the IMMEDIATE children
    (sub-folders + files) of a validated plaintext prefix from the range-scan rows.
    Folders sort before files. Names are plaintext (file_state stores the plaintext
    path even for encrypted sources, SPEC s2), so the browser shows decrypted names
    with no keystore touch for display.
  - `search_files(source_id?, query, limit)`: routes a glob-metachar query to the
    GLOB path, else FTS5.
  - `restore_files(items, dest_token)`: validates + resolves each selection to its
    authoritative file_state row, builds the per-account remote store + crypto
    verdict UP FRONT, then SPAWNS a background job that streams `restore:progress`.
  - `get_restore_job(job)`: serves the latest snapshot from an AppState registry
    for a late subscriber.

- **Streaming decrypt (DATA-SAFETY + PERF).** `stream_to_disk` reads the 40-byte
  header, opens a `ContentDecryptor`, and decrypts ONE 64-KiB+tag ciphertext frame
  at a time using a rolling buffer: it `decrypt_chunk`s a leading frame only while
  STRICTLY MORE than one frame is buffered (so the frame is provably non-final),
  and `decrypt_last`s whatever <= one frame remains at EOF. RSS stays ~2 frames
  (~128 KiB) regardless of file size, so a 1 GiB encrypted file never sits whole in
  RAM. The restored plaintext is BLAKE3-verified against `file_state.hash_blake3`
  (a wrong decrypt / corrupted object is refused, not written) and the length is
  cross-checked, then placed via temp + fsync + atomic rename. Tests: multi-chunk
  (~5.3 MiB) round-trip, empty/small, exact-frame-multiple edge case, plaintext
  passthrough, wrong-key failure, blake3-mismatch refusal, filename decrypt
  round-trip.

### Path-validation approach (SPEC s11.6.1)

The restore destination is the untrusted input. The webview never sends a raw
path: it calls `pick_folder_dialog` (backend-owned native picker) which mints a
one-shot dialog TOKEN; `restore_files` CONSUMES the token to the approved root,
then a new `validate_restore_dest(token, relative_path)` confines every per-file
write under that root. It reuses `RelativePath::try_from` (rejecting `..`,
absolute, drive/UNC, NUL), canonicalises the dialog root, joins the relative tree,
`create_dir_all`s the parent chain, RE-canonicalises the parent and re-confirms it
is still under the root (catches a pre-existing symlinked-directory escape), and
refuses a symlink-at-leaf. The `list_remote_tree` prefix is validated as a
printable, `/`-separated, length-bounded plaintext path (NOT a local path). Tests
in `commands/mod.rs`: nested-dir creation + confinement, traversal rejected,
absolute rejected, symlink-at-leaf rejected, symlinked-parent-escape rejected
(unix).

### Frontend

`Restore.vue` (replacing the M6 placeholder): source selector, search box,
breadcrumb tree (lazy per folder), multi-select with checkboxes, backend folder
dialog for the destination, restore button, and a live progress panel (overall
bar + per-file states) driven by the `restore` Pinia store subscribing to
`restore:progress`. Typed IPC wrappers + `RestoreJobStatus`/`RemoteEntryDto`/...
DTO types mirror the Rust serde shapes (camelCase). vitest drives the whole
browse -> search -> select -> restore flow against mocked `invoke` + a mocked
`restore:progress` stream (progress accumulates to a terminal `done`).

### Deviations / residuals

- **`restore_files` takes a dialog `dest_token`, not the SPEC s11.5 raw
  `dest_dir: PathBuf`.** This is the SAME deviation s11.6.1 MANDATES and the M6
  surface already adopted for `export_diagnostic_bundle`/`add_source`: a raw
  webview path is forbidden, so the signature carries the one-shot token the
  backend resolves to the approved directory. The contract is documented on the
  command + the typed wrapper.
- **ICU plural not used for `restore.selectedCount`.** vue-i18n's default message
  compiler in this repo is not configured for ICU MessageFormat (DESIGN s8.7 names
  it as the V1 target but it is not wired; no existing key uses it). Used a plain
  `{count} selected` interpolation rather than introduce an unparseable ICU string
  that breaks the build. Wiring ICU is a cross-cutting i18n change, out of M8 scope.

## M8 codex round-1 fixes (2 P1 + 5 P2; baseline 1a7ad60 @ 887aaab)

All findings from `.claude/codex-reviews/M8-20260624-161042.md` fixed. The prior
"no mid-job cancel command" residual is now RESOLVED (P1-1).

- **M8-P1-1 - cancellable restore + cleanup + shutdown drain.** Each restore job
  now holds a shared cancel flag (`Arc<AtomicBool>`) + its spawned `tokio`
  `JoinHandle`, tracked on `AppState` (`seed_restore_job` / `set_restore_job_handle`
  / `cancel_restore_job` / `cancel_all_restore_jobs` / `finish_restore_job_handle`).
  New `cancel_restore_job(jobId)` IPC sets the flag; `stream_to_disk` checks it
  BEFORE each file and between frames, and on cancel returns `Cancelled` so
  `restore_one_file` DELETES the temp (no partial, nothing renamed into place). The
  job emits a terminal CANCELLED `RestoreJobStatus` (`cancelled: true`,
  `RestoreFileState::Cancelled` for the unfinished files). App shutdown
  (`lib.rs::shutdown_orchestrators`) now `cancel_all_restore_jobs()` + joins every
  restore handle alongside the M5 account drain, so quit leaves no orphaned restore
  task and no partial files. UI: a Cancel button (gated by a `cancelling` flag), a
  `cancelled` terminal label, and a per-file Cancelled state.
- **M8-P1-2 - no-follow, non-truncating, race-safe temp write.** The temp is now a
  RANDOM name (`.driven-restore-tmp.<uuid>`, not timestamp-derived) opened via
  `open_temp_no_follow`: `create_new(true)` (O_EXCL - fails if the path exists,
  killing a pre-place / race-to-the-path attack) PLUS platform no-follow flags
  (Unix `O_NOFOLLOW`; Windows `FILE_FLAG_OPEN_REPARSE_POINT`) so a symlinked temp
  leaf cannot redirect the write outside the approved root. After the stream the
  rename target is RE-validated via `validate_restore_dest` (catches a TOCTOU leaf
  swap) before renaming. Tests: O_EXCL rejects an existing path; a pre-placed
  symlink at the temp path is refused and the victim target is not overwritten.
- **M8-P2-1 - surface tree truncation.** `list_remote_tree` now returns
  `RemoteTreeDto { entries, truncated }` instead of a bare `Vec`; `truncated` is set
  when the folder has more immediate children than the cap (or the scan hit its row
  cap). Restore.vue shows a "showing the first N items" notice. The cap itself is
  unchanged.
- **M8-P2-2 - search input limits per DESIGN s18.8.** `MAX_QUERY_LEN` tightened
  from 1024 to 256 (counted in CHARS), and `search_files` now rejects `\0`, `\r`,
  `\n`, and any other control char before the FTS/GLOB path.
- **M8-P2-3 - bounded restore-job snapshot memory.** Terminal restore-job records
  are TTL-pruned (1h) and count-capped (32, oldest-terminal first) by
  `prune_terminal_jobs` on every register/put; active jobs are never pruned.
- **M8-P2-4 - persist active job id + reconcile on remount.** The store keeps the
  returned `jobId` and calls `getRestoreJob(jobId)` after start AND on
  (re)subscription, so a remount / missed terminal event recovers current state
  instead of going stale.
- **M8-P2-5 - classify remote download failures.** `classify_download_error` maps
  the Drive `download` error into the specific SPEC s24 code (auth.invalid_grant /
  drive.rate_limited / drive.daily_quota_exhausted / drive.quota_exhausted /
  net.intermittent / drive.unreachable for 404/unclassified), reusing the same
  typed `classification_of` downcast + string fallback the executor uses. The
  specific code is stored on the per-file restore failure.

## M8 codex recheck-1 fixes (2 P1 + 2 P2; baseline 1a7ad60 @ dc428fb)

All findings from `.claude/codex-reviews/M8-recheck1-20260624-165457.md` fixed.

- **R1-P1-1 - confine the parent chain BEFORE creating it (symlink escape).**
  `validate_restore_dest` (`commands/mod.rs`) previously called
  `std::fs::create_dir_all(parent)` BEFORE the symlink/confinement check, so a
  pre-existing symlink directory component in the restore root (`escape ->
  C:\outside`) let `create_dir_all` create directories OUTSIDE the approved root
  (`C:\outside\new`) before the post-hoc canonicalise-and-reject ran (SPEC s11.6.1
  violation). Now it WALKS the destination directory components ONE AT A TIME from
  the canonical root: at each existing component it rejects a SYMLINK (and re-confirms
  the canonical form is still under the root) BEFORE descending, and for a missing
  component it creates ONLY that one level (`std::fs::create_dir`, never
  `create_dir_all`) once the current parent is verified real + confined. No directory
  is ever created outside the canonical root. Test (`validate_restore_dest_symlink_
  component_creates_no_dir_outside_root`, Unix): restoring `escape/new/file.txt`
  through a symlinked `escape` is rejected AND `outside/new` is NOT created.
- **R1-P1-2 - UI cancel must not orphan the task.** `cancel_restore_job` (the IPC)
  called `AppState::cancel_restore_job`, which TAKES the spawned `JoinHandle` out of
  the job entry; the IPC then `let _ = ...` DROPPED it, detaching/orphaning the
  tokio task (after a UI cancel the task was no longer drainable on shutdown and
  could run on until its next read). Added `AppState::signal_cancel_restore_job`,
  which ONLY sets the cancel flag and LEAVES the handle tracked; the IPC now uses it.
  The task still observes the flag, cleans its temp, emits CANCELLED, and clears its
  own handle on exit (`finish_restore_job_handle`); the M5-style shutdown drain
  (`cancel_all_restore_jobs`) still finds + awaits/aborts the handle, so a UI cancel
  never detaches the task. The handle-taking `cancel_restore_job` is retained as
  public API for an owning caller. Test (`ui_cancel_sets_flag_but_keeps_handle_for_
  shutdown_drain`): after a UI cancel the handle is STILL tracked and joinable by the
  shutdown drain (not orphaned).
- **R1-P2-1 - classify mid-stream download read errors.** Mid-stream Drive download
  (`AsyncRead`) read errors were mapped through `map_io_err`, so a network/timeout
  became `local.io_error` (the user was told the DISK failed when DRIVE/network
  failed). Added `driven_drive::google::classify_stream_read_error(&io::Error)`,
  which walks the io error's source chain for the wrapped `DriveError` / raw
  `reqwest::Error` (the real `StreamingDownloadReader` wraps the transport error via
  `io::Error::other`) and returns its `DriveErrorClassification`. `restore.rs`'s new
  `map_download_read_err` routes every download-stream READ error (the plaintext +
  encrypted read loops, and the encrypted header `read_exact` except a genuine
  `UnexpectedEof`, which stays `crypto.decrypt_failed` for a truncated object) to the
  right s24 code (net.intermittent / drive.rate_limited / auth / quota / ...), never
  `local.io_error`; disk WRITE errors still use `map_io_err`. Tests: a mid-stream
  read break maps to net.intermittent (unclassified) / drive.rate_limited (dotted-
  code message), and the mapper never returns local.io_error.
- **R1-P2-2 - atomic replace-over-existing on Windows.** `tokio::fs::rename(&tmp,
  &dest)` is not a portable atomic REPLACE: restoring OVER an existing file can fail
  on Windows. Added `atomic_replace` (`restore.rs`) using the platform primitive on
  a blocking thread - Windows `MoveFileExW(MOVEFILE_REPLACE_EXISTING |
  MOVEFILE_WRITE_THROUGH)` (new Windows-only `windows-sys` dep, `cfg(windows)`-gated
  so the 3-OS CI stays green), Unix `std::fs::rename` (already replaces). Tests:
  `atomic_replace` overwrites an existing file (temp gone after), and a full
  `restore_one_file` over a pre-existing dest succeeds + leaves no temp.

  > Superseded by R2-P1-1 (recheck-3): `atomic_replace` + the by-path
  > `open_temp_no_follow` were REPLACED by the handle-confined `confine::ConfinedDest`
  > (the temp create + the rename are now performed against a PINNED no-follow
  > parent handle). The replace-existing semantics are preserved (Unix `renameat`
  > replaces; Windows `FILE_RENAME_INFO { ReplaceIfExists }`).

## M8 codex recheck-2 fixes (2 P1 + 2 P2; baseline 1a7ad60 @ e2e84b4)

All findings from `.claude/codex-reviews/M8-recheck2-20260624-173857.md` fixed.

- **R2-P1-1 - final restore confinement was a path-string TOCTOU; now HANDLE-based.**
  `restore_one_file` (`commands/restore.rs`) validated the dest path, then DISCARDED
  the verified path and re-resolved the dest STRING to (a) open the temp by full path
  and (b) `atomic_replace(&tmp, &dest)` at rename time. A local process could swap a
  parent directory component to a symlink/junction AFTER validation and BEFORE the
  open/rename, redirecting the decrypted restore plaintext (temp AND final) OUTSIDE
  the dialog-approved root (SPEC s11.6.1 data-safety violation). Note the temp itself
  - not just the rename - held out-of-root plaintext for the WHOLE stream, a far
  bigger window than the rename alone.

  Fixed with a new `mod confine` (`ConfinedDest`) that pins the parent chain with a
  no-follow directory HANDLE and performs BOTH the temp create and the final rename
  against that pinned handle (NOT a re-resolved string):
  - **Unix (`rustix`, new `cfg(unix)` dep):** `open` the canonical root no-follow as a
    directory fd, then `openat` each directory component with `O_NOFOLLOW | O_DIRECTORY`
    (a swapped symlink component fails the open with ELOOP/ENOTDIR), arriving at a
    pinned final-parent fd. The temp is created via `openat(parent_fd,
    O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC)` and the commit is
    `renameat(parent_fd, temp, parent_fd, leaf)` (atomic replace). Both are
    handle-relative, so a parent swap after validation cannot move the write out of
    root. Tests (`#[cfg(unix)]`): a post-validation parent swap to an out-of-root
    symlink, and a directly-symlinked leaf-parent, are BOTH rejected and create NO
    file (temp or final) outside root.
  - **Windows (`windows-sys`, existing `cfg(windows)` dep + `Win32_Security` feature):**
    open the parent chain no-follow via `CreateFileW(BACKUP_SEMANTICS |
    OPEN_REPARSE_POINT)`, rejecting any component whose handle reports
    `FILE_ATTRIBUTE_REPARSE_POINT` (a junction/symlink) and re-confirming each
    handle's `GetFinalPathNameByHandleW` resolved path stays in-root. The temp is
    created `CREATE_NEW` (O_EXCL: refuses a pre-place race) + `DELETE` access +
    `OPEN_REPARSE_POINT`, then its handle's resolved path is re-checked in-root BEFORE
    any plaintext is written. The commit derives the dest path from the PINNED PARENT
    HANDLE's CURRENT `GetFinalPathNameByHandleW` resolved real path (NOT the original
    attacker-influenceable string) and renames via `SetFileInformationByHandle(
    FileRenameInfo { RootDirectory: NULL, FileName: <resolved_parent>\\<leaf>,
    ReplaceIfExists: TRUE })` on an 8-byte-aligned buffer. Because the parent handle
    is pinned to the REAL directory inode, a swapped-in junction does not move the
    rename target: the resolved path is the real pinned dir, never the junction's
    target.

  **Documented Windows residuals (strongest achievable without the NT API, per the
  fix-spec):**
  1. Win32's `SetFileInformationByHandle(FileRenameInfo)` does NOT support a non-NULL
     `RootDirectory` (it returns ERROR_INVALID_PARAMETER - the handle-relative form is
     `NtSetInformationFile`-only). So the Windows rename is confined by RESOLVING the
     pinned parent handle (not by a RootDirectory-relative move). This is TOCTOU-safe
     (the pinned handle resolves the real inode), but is not the literal
     RootDirectory-relative primitive the Unix path uses. Closing this gap would
     require ntdll plumbing, deliberately avoided as itself a data-safety risk.
  2. The Windows temp is created by full path then IMMEDIATELY re-checked in-root, so
     a junction swapped in at the exact create instant could momentarily produce an
     EMPTY (zero-plaintext) temp out-of-root; it is detected and deleted BEFORE byte
     one, so NO plaintext ever leaves root.
  3. `FILE_RENAME_INFO` has no `WRITE_THROUGH` (the file DATA is already `sync_all`'d
     by the streamer; only the rename METADATA is not flush-forced - a crash
     immediately after rename could lose the rename, never corrupt data).

  Abort-safety (ties into R2-P2-2): `ConfinedDest` removes its temp on `Drop` if not
  committed (Unix `unlinkat` handle-relative; Windows `remove_file` by path), so a
  failure, a cancel, OR a shutdown ABORT that drops the streaming future leaves no
  temp / out-of-root plaintext. `commit()` defuses the guard only after the rename
  succeeds. Tests: `confined_commit_replaces_existing_and_confines` (handle-confined
  replace over an existing dest, no temp left) and `confined_dest_drop_without_commit_
  removes_temp` (drop-without-commit removes the temp, no final file).

- **R2-P1-2 - route simple trailing-star prefix searches to FTS5, not GLOB.**
  `is_glob_query` (`restore.rs`) treated ANY `*` as a glob, so a plain trailing-star
  prefix term like `proj*` was sent to the slow SQLite `GLOB` scan and never reached
  the FTS5 prefix logic already in `build_fts_match_query` (which emits the `"proj"*`
  prefix form) - missing ROADMAP M8's `<50ms` prefix acceptance. The dispatcher now
  routes to GLOB only for GENUINE wildcard / path patterns (any `?`/`[`/`/`, a leading
  or interior `*`, a bare/empty-stem `*`/`**`, or multiple `*`), and routes a SINGLE
  trailing-star term over a non-empty stem (and plain terms) to FTS5. Tests:
  `proj*`/`taxes-2025*` route to FTS5; `*.rs`, `src/*`, `a*b`, `a?b`, `[ab]c`, `*`,
  `**`, `a*b*` route to GLOB. (The FTS5 prefix machinery itself is already covered by
  `driven-core`'s `search_files_prefix_*` tests.)

- **R2-P2-1 - do not seed/emit a restore job before fallible setup succeeds.**
  `restore_files` seeded + emitted the job BEFORE the fallible plan construction
  (`list_sources`, `build_restore_store`, source lookup); an early `Err` returned but
  left a non-terminal job with no handle in `AppState` (never pruned) + a dangling
  "running" job event. The fallible setup is now extracted to `build_restore_plans`
  (which NEVER touches the job map) and runs BEFORE `seed_restore_job` + the first
  emit, so a setup failure returns `Err` with no job created. Test
  (`build_restore_plans_failure_leaves_no_lingering_job`): an unknown-source setup
  fails and `AppState.restore_jobs` stays empty (new test-only `restore_jobs_len`).

- **R2-P2-2 - bounded, abort-capable shutdown drain for restore jobs.** `lib.rs`'s
  quit sweep awaited every restore `JoinHandle` with NO timeout/abort, so a job stuck
  BEFORE it observed its cancel flag hung an explicit Quit forever. Added
  `drain_restore_handle` (bounded `RESTORE_JOB_DRAIN_TIMEOUT` = 5s): await each handle
  up to the budget, then `abort()` + await the aborted handle so the task is truly
  gone - mirroring the M5 per-account `drain_or_abort`. Temp cleanup is abort-safe via
  the `ConfinedDest` Drop guard above (dropping the aborted future removes the temp).
  Tests (virtual-time `start_paused`): a stuck task is aborted within the budget (not
  allowed to finish its long sleep); a prompt task is joined cleanly without abort.

## M8 codex recheck-3 fixes (round 4: 2 P1 + 3 P2, FINAL - cap-4 hard stop)

All findings from `.claude/codex-reviews/M8-recheck3-20260624-183342.md` (baseline
1a7ad60, M8 @ 209bc1e) fixed. This was the FINAL fix round under the user-granted
cap of 4: after codex recheck-4 M8 closes regardless, and any recheck-4 residual is
documented here / pushed to M9 pre-GA hardening (no round 5).

- **R3-P1-1 - detect restore target collisions (DATA-SAFETY).** The UI kept the
  multi-selection across source switches and the backend restored every selected
  item to `dest/<relative_path>`, so two sources both selecting `foo.txt` - or
  `Foo.txt`+`foo.txt` on a case-insensitive dest - SILENTLY overwrote each other.
  The BACKEND is now the real guard: before consuming the dialog token or spawning
  the job, `detect_dest_collisions` (`restore.rs`) computes each selected item's
  destination KEY and REJECTS the whole job (`internal.invalid_input`, naming the
  two conflicting paths) on (a) a duplicate folded key or (b) a file-vs-directory
  path-prefix conflict (one item's key is a strict, SEGMENT-WISE ancestor of
  another's - never a raw `starts_with`, so `foo` does not falsely prefix
  `foobar`). Case folding is the documented DEFAULT (each segment lowercased): a
  case-insensitive dest is the norm on the supported platforms (Windows ALWAYS,
  macOS/APFS by default), and an over-reject is a visible error while an
  under-reject is silent data loss - so we DO NOT probe the dest's case
  sensitivity (a probe would create throwaway files in the user's folder for an
  edge that does not arise on the supported platforms). Defense in depth: the
  store (`restore.ts` `selectSource`) clears the selection when the ACTIVE source
  changes, so cross-source selections do not silently accumulate. Tests:
  `detect_dest_collisions_rejects_same_destination_key`,
  `detect_dest_collisions_folds_case_on_insensitive_dest`,
  `detect_dest_collisions_rejects_file_vs_dir_prefix_conflict`,
  `detect_dest_collisions_allows_non_colliding_multisource_selection`; UI
  `surfaces a backend destination-collision rejection...` + `clears the
  cross-source selection when the active source changes`.

- **R3-P1-2 - atomic seed+spawn+handle vs shutdown (no orphan / no partial temp).**
  The job was seeded (status+cancel), THEN spawned, THEN the `JoinHandle` attached
  in a separate `set_restore_job_handle` call - a window where a quit's
  `cancel_all_restore_jobs` saw a seeded job with NO awaitable handle, and if the
  task had already created a temp the process could exit mid-write leaving a
  partial. Fixed with a START BARRIER: the task is spawned but gated behind a
  `tokio::sync::oneshot` (`release_rx.await`) so it does NO filesystem work until
  released; `seed_restore_job` now takes the `JoinHandle` and inserts status +
  cancel + handle in ONE locked insert (so a seeded job is NEVER observable without
  an awaitable handle); only THEN is the barrier released, and the task re-checks
  the cancel flag immediately on release (via `run_restore_job`'s pre-file check)
  so a quit/cancel in the window exits clean with no temp. `set_restore_job_handle`
  is removed (the atomic seed replaces it). Net invariant: the shutdown drain never
  sees a handle-less seeded job, and a quit anywhere in the spawn window leaves no
  partial temp. Test: `seeded_restore_job_always_has_awaitable_handle_for_shutdown_
  drain` (seed-with-handle, then a quit BEFORE release finds the handle and the
  gated task does no fs work / leaves no marker).

- **R3-P2-1 - do not burn the one-shot dest token on a pre-acceptance failure.**
  `restore_files` CONSUMED the token before validating the selection / building
  plans, so any stale-selection / unuploaded-row / collision / keychain / setup
  error burned the token and forced the user to re-pick the folder. CHOSEN
  CONTRACT (documented): peek-early / take-late. The command now PEEKs the token
  (`peek_dialog_token`, non-consuming) to resolve+validate the dest dir, runs ALL
  token-independent validation (resolution + eligibility + collision check) and the
  fallible `build_restore_plans` setup, and CONSUMES the token (`take_dialog_token`)
  only immediately before the atomic seed/spawn - the first + only irreversible
  step. So every pre-spawn failure leaves the token INTACT; no re-pick signal is
  needed. (The peeked path equals the bound path, so the consume is purely to spend
  the single use + bound replay.) Covered by the resolve/collision tests above (all
  fail BEFORE the consume) plus the existing single-use token test.

- **R3-P2-2 - reject unuploaded / non-restorable rows as bad input.** A stale /
  malicious renderer could queue a row with `drive_file_id = NULL` (never uploaded)
  or a non-`synced` status; these flowed in and failed later as `internal.bug`.
  `resolve_restore_items` (`restore.rs`) now REJECTS, with `internal.invalid_input`
  before any job is spawned: an unknown PK row (a stale/forged selection - widened
  from `internal.bug`), a row with NULL `drive_file_id`, and a row whose status is
  not `synced`. ELIGIBILITY SEMANTICS (documented): restore requires an uploaded
  object AND `status == Synced`. We deliberately do NOT restore a non-`synced` row
  (pending/error/corrupt/locked): its recorded `hash_blake3` may not match the
  bytes currently on Drive, so the restore would either fail the in-stream BLAKE3
  verify late (a confusing `crypto.decrypt_failed`) or hand back a mismatched
  object - rejecting up front is the honest, visible behaviour. Tests:
  `resolve_restore_items_rejects_null_drive_file_id_as_bad_input`,
  `resolve_restore_items_rejects_non_synced_row_as_bad_input`,
  `resolve_restore_items_accepts_clean_synced_selection`.

- **R3-P2-3 - query immediate tree children in SQL, not first-N descendants.**
  `list_remote_tree` derived immediate children from the first `MAX_TREE_SCAN_ROWS`
  DESCENDANT rows, so a first sub-folder holding 100k+ files exhausted the scan and
  HID later sibling folders/files (the UI saw only `truncated`). Added
  `StateRepo::list_immediate_tree_children` (SQLite override) which queries
  IMMEDIATE children directly with two capped INDEXED range-scans over the same
  `[prefix/, prefix0)` bounds: immediate FILES (local remainder has no `/`, via
  `instr(substr(...),'/')=0`) and DISTINCT first-segment SUB-FOLDERS (deeper rows,
  `substr(...,1,instr(...)-1)`). Children are capped (cap+1 fetched per kind);
  `truncated` is set ONLY on a genuine immediate-CHILD overflow, never because one
  sub-folder is large. `MAX_TREE_SCAN_ROWS` is gone; `derive_immediate_children`
  is now `#[cfg(test)]` (its split/ordering semantics are now enforced by the SQL).
  New `.sqlx` query files committed (sqlx-prepare, 0 drift). Tests (SqliteStateRepo,
  real DB): `immediate_tree_children_lists_direct_folders_and_files`,
  `immediate_tree_children_does_not_hide_siblings_behind_huge_first_folder` (the
  discriminating test - a 1500-file first folder no longer hides the later sibling
  folder/file), `immediate_tree_children_truncates_on_child_count_overflow`.

### Residual / not-fixed (recheck-3)

None. All 2 P1 + 3 P2 fixed with tests + full gates green (cargo build/clippy/test/
deny/fmt + ui lint/test/vue-tsc/build). The Windows temp create-instant junction
window and the `FILE_RENAME_INFO` no-WRITE_THROUGH note remain as previously
documented under R2-P1-1 (no plaintext ever leaves root; data is `sync_all`'d) -
unchanged by this round.

## M8 codex recheck-4 (FINAL) - ACCEPTED RESIDUALS; M8 review CLOSED

The codex recheck-4 (`high`, baseline 1a7ad60, M8 @ 97f596e, CI + Chaos GREEN,
vue-tsc clean) raised **1 P1 + 2 P2** -
`.claude/codex-reviews/M8-recheck4-20260624-192136.md`. This is the cap-4 hard stop
the user set when granting the 2 bonus rounds: M8 CLOSES regardless, and these
recheck-4 findings are ACCEPTED RESIDUALS deferred to **M9 pre-GA hardening + the
pre-GA xhigh whole-repo security capstone (task #14)** - NOT a recheck-5. This
mirrors the M6 recheck-4 precedent (an even more severe data-safety item -
recovery-phrase ack -> unrestorable backups - was likewise documented and deferred
to M9 pre-GA rather than looped on). The recheck cycle was NOT converging (each
round surfaced NEW restore.rs edge cases), so remaining restore hardening is better
done holistically in the pre-GA pass than in another reactive single-finding round.

- **R4-P1-1 (restore.rs commit() verify->rename TOCTOU) - DATA-SAFETY GA-BLOCKER,
  deferred to M9 pre-GA.** The restored bytes are BLAKE3-verified through the OPEN
  temp file, but `commit()` drops/closes that handle and then renames BY TEMP
  NAME/PATH (Unix `renameat(parent, temp_name, ..)`; Windows re-opens from
  `c.temp_path`). A local process watching the dest dir can unlink/replace
  `.driven-restore-tmp.<uuid>` AFTER verification and BEFORE the rename, so the final
  file can hold attacker-controlled bytes that never passed verification (silent
  restore corruption). Narrow local race (needs an attacker process actively winning
  the verify->commit window), but real. FIX (for M9 pre-GA): keep the temp HANDLE
  owned through commit and commit/verify the SAME object - Windows
  `SetFileInformationByHandle(FileRenameInfo)` on the original temp handle; Unix keep
  the temp fd, verify final/name identity against the open fd before defusing the
  cleanup guard (prefer an fd-based link-by-fd strategy where available). Do NOT
  trust the temp pathname after verification. **GA is gated on this** (M9 pre-GA +
  capstone #14 xhigh over the restore path will re-flag it if unfixed).

- **R4-P2-1 (restore.rs:209/:461 + :1442) shared restore-eligibility predicate.**
  The tree/search DTOs mark a row `restorable` whenever `drive_file_id` is present,
  but restore resolution rejects anything whose status != `Synced` (R3-P2-2), so a
  changed/pending/error row with an old Drive id LOOKS selectable then fails only at
  restore start. Fix (M9 pre-GA): one shared eligibility predicate
  (`drive_file_id.is_some() && status == Synced`) used by BOTH the tree/search DTO
  `restorable` flag and restore resolution, so the UI never offers an ineligible row.

- **R4-P2-2 (ui/src/stores/restore.ts:268) stale activeJobId on rejected start.**
  `startRestore()` clears `job` but leaves `activeJobId` from the prior restore until
  the new IPC succeeds; if the new restore is REJECTED (e.g. a collision/bad-input
  error from the R3 fixes), the store still tracks the OLD job id, so a later
  reconcile/cancel can target stale state. Fix (M9 pre-GA): set `activeJobId = null`
  BEFORE calling `restoreFiles()`, assign the returned id only on success.

M8 is CLOSED. Restore flow (list_remote_tree / search_files / streaming-decrypt
restore_files + Restore.vue) ships with these 3 documented residuals folded into the
M9 pre-GA hardening scope; the R4-P1-1 TOCTOU is the data-safety GA-blocker.

## M9c - data-safety hardening

The M9c pre-GA pass fixed the 4 CRITICAL data-safety residuals carried from M6 +
M8 (the M8 recheck-4 trio + the M6 recheck-4 recovery-phrase P1). Disjoint from
M9 r3 (release pipeline) - touches ONLY `src-tauri/src` (restore + recovery +
commands + app_state) and `ui/` (restore store + recovery-phrase reveal + the two
wizards). All gates green on Windows (cargo build/clippy -D/test --lib 157 passed;
deny/fmt/git diff --check); `cd ui` pnpm install/lint/test:unit (133 passed)/build
(vue-tsc clean). The Unix-only D1 swap-detection test is cfg-gated (runs on the
3-OS CI Linux/macOS legs). Stub sweep on the touched surface: zero non-test
`todo!`/`unimplemented!`/`unreachable!`.

- **D1 (M8 R4-P1-1) - restore commit() verify->rename TOCTOU [DATA-SAFETY
  GA-BLOCKER] FIXED.** `commit()` previously dropped the BLAKE3-verified temp
  handle and renamed BY temp NAME/PATH (Unix `renameat`; Windows re-opened the
  temp from `c.temp_path`), so a local process could unlink/replace the temp
  between verification and rename and get unverified bytes committed (silent
  restore corruption). Fix: `ConfinedDest` now RETAINS the temp file's OWN handle
  from create through commit; the streamer writes + verifies through a `try_clone`
  DUP, and the rename acts on the RETAINED, verified object.
  - Windows: `SetFileInformationByHandle(FileRenameInfo)` is now called on the
    RETAINED temp handle (opened with `DELETE`), NOT a re-open of `temp_path`. A
    swap at the path cannot redirect the rename - the handle still names the
    original inode.
  - Unix: before `renameat` the commit fstat's the retained temp fd and
    `statat(..., SYMLINK_NOFOLLOW)`'s the temp name in the pinned parent, and
    proceeds ONLY if `(st_dev, st_ino)` match; a detected swap FAILS the commit
    (`local.io_error`) - no silent corruption.
  - Residual (documented): on Windows the rename DESTINATION is still derived from
    the pinned PARENT handle's resolved path (the handle-relative `RootDirectory`
    form of FILE_RENAME_INFO is NT-API-only), so the parent-pin - not a dest
    string - confines the target dir; the SOURCE object is now fully
    handle-pinned. `FILE_RENAME_INFO` still has no WRITE_THROUGH (file DATA is
    `sync_all`'d; only the rename metadata is not flush-forced). Tests:
    `confined_commit_uses_retained_handle_after_streamer_drops_its_dup`,
    `restore_one_file_committed_bytes_equal_verified_bytes` (both OS),
    `confined_commit_rejects_temp_swapped_after_verification` (Unix).
- **D2 (M8 R4-P2-1) - shared restore-eligibility predicate FIXED.** Added ONE
  `is_restorable(status, drive_file_id) = drive_file_id.is_some() && status ==
  Synced` used by the tree DTO mapper, the search DTO mapper, AND restore
  resolution (`resolve_restore_items`). The DTOs no longer mark a non-synced row
  (with a stale Drive id) restorable, so the UI never offers a row resolution then
  rejects. Tests: `is_restorable_requires_synced_status_and_a_drive_id`,
  `tree_dto_marks_non_synced_row_not_restorable`,
  `resolve_and_dto_agree_on_eligibility_via_one_predicate`.
- **D3 (M8 R4-P2-2) - stale activeJobId on rejected restore FIXED.** `startRestore`
  now sets `activeJobId = null` BEFORE calling `restoreFiles`, assigning the
  returned id only on success - so a rejected new restore no longer leaves the
  store tracking the PRIOR job id (stale reconcile/cancel). Vitest:
  `clears a STALE activeJobId when a new restore is rejected`.
- **D4 (M6 R4-P1-1) - recovery-phrase backend-ACK [DATA-SAFETY] FIXED (P1 only;
  the 6 M6 P2s remain deferred).** Root-cause fix per the documented residual: the
  FIRST encrypted source (the add that generates the master key) is now persisted
  `enabled:false` and registered pending-ack on `AppState`; it is EXCLUDED from the
  scheduler + manual sync (both filter on `enabled`), so no encrypted backups run
  before the recovery phrase is durably saveable. Two new backend commands gate
  enabling it: `reveal_recovery_phrase(source_id)` re-derives the phrase from the
  keychain master key and RECORDS a real backend reveal; `ack_recovery_phrase_saved
  (source_id)` is REJECTED unless that reveal was recorded, and on success flips
  `enabled:true` + reconfigures the account. `add_source` returns a new
  `pending_recovery_ack` flag; the wizard (`AddSourceWizard` + `SetupWizard`) thread
  a backend reveal action into `RecoveryPhraseReveal` (which now awaits it and
  latches "revealed" only on success) and call the backend ack on Finish before the
  initial sync. So a UI checkbox can never enable encrypted backups without a real
  backend reveal. Tests: backend `recovery_ack_gate_requires_a_recorded_backend_reveal`
  (the AppState gate the command enforces); vitest reveal-action latch +
  reject-leaves-locked. New i18n key `recoveryPhrase.revealingButton`.

## M9d - release pipeline

Authored the release / ops pipeline (DISJOINT from M9a's updater feature): the
tag-triggered build/sign/publish workflow, release-please automation + config,
the rolling dev channel, the Cloudflare Pages /updates wiring, and the chaos
real-Drive flip. NOTHING under src-tauri/src, ui/, tauri.conf.json, Cargo.toml,
or ui/package.json was touched (those are M9a/M9b/M9c).

### Shipped

- `.github/workflows/release.yml` - trigger `push: tags: ['v*']`. Build matrix
  (macos aarch64 + x86_64, ubuntu-22.04 x86_64, windows x86_64) via
  tauri-apps/tauri-action@v0 with GITHUB_TOKEN + TAURI_SIGNING_PRIVATE_KEY +
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD; `args: --target <matrix.target>`;
  `uploadUpdaterJson: false` (tauri-action's flat latest.json does NOT match
  Driven's channel-in-path layout, so we generate the tree ourselves; the .sig
  files still upload). A `publish-updater-manifest` job (needs: build) runs
  `node scripts/generate-update-json.mjs stable`, attaches the per-target
  manifests to the GH Release, and `wrangler pages deploy updates` to CF Pages
  `driven-updates`.
- `.github/workflows/release-please.yml` - on push main,
  googleapis/release-please-action@v4 with config-file + manifest-file;
  `contents: write` + `pull-requests: write`. Maintains the chore release PR;
  merging it tags v* -> fires release.yml.
- `release-please-config.json` + `.release-please-manifest.json` - manifest mode.
- `.github/workflows/dev-channel.yml` - rolling 0.0.0-dev.<short-sha> dev channel
  (gated, see tradeoff below). Same 4-row matrix, uploads to a rolling `dev`
  pre-release via softprops/action-gh-release@v2, generates updates/dev/... and
  deploys to CF Pages.
- `chaos.yml` - flipped the `chaos-real-drive` job from `if: ${{ false }}` to
  `if: ${{ startsWith(github.ref, 'refs/tags/') }}` so the v* tag exercises the
  real-Drive chaos suite (the DRIVEN_E2E_* secrets now exist). The chaos matrix
  was NOT otherwise expanded: hermetic / fake-drive stay windows-only on PR/main,
  3-OS only on tags; real-drive is a single ubuntu leg added only on tags.

### release-please-type choice

`release-type: rust` for the root (`.`) package. Driven's canonical version lives
in the Cargo workspace (`[workspace.package].version` in the root Cargo.toml;
src-tauri/Cargo.toml uses `version.workspace = true`), which the Rust strategy
understands and bumps natively, and it generates the Rust-flavored CHANGELOG. The
two non-Rust version mirrors (`src-tauri/tauri.conf.json` and `ui/package.json`)
are bumped via per-package `extra-files` entries using the typed JSON updater
(`{type: json, path, jsonpath: "$.version"}`). NOTE: for `.json` extra-files the
typed form is REQUIRED - unlike `.yaml`, a bare-string `.json` extra-file does
NOT auto-apply a `$.version` updater (it only runs the comment-marker Generic
updater, and there are no `x-release-please-version` markers in those files), so
bare-string entries would silently fail to bump. extra-files are placed INSIDE
the package block (manifest-mode requirement), not at top level.

### dev-channel trigger tradeoff (COST POLICY)

The full 4-row (3-OS) Tauri bundle is expensive (premium macOS/Windows minutes).
Firing it on EVERY main push would silently burn the CI budget on commits that
have no business producing a dev installer. So dev-channel.yml does NOT build on
every push: a `gate` job decides `build=true` only when (a) it is manually
dispatched (`workflow_dispatch`), OR (b) the head commit message contains the
explicit `[dev-build]` marker. It also explicitly skips release-please's
"chore(main): release" / "release" commits so it never double-fires with the
release.yml tag path. release.yml's full matrix is acceptable precisely because
it is tag-only (once per tagged release). If cadence-based dev builds are wanted
later, prefer a `schedule:` (nightly) over an every-push trigger.

### Cloudflare Pages whole-site-snapshot caveat (IMPORTANT for M9a's script)

`wrangler pages deploy <dir>` replaces the ENTIRE site snapshot - it is not an
incremental upload. Driven serves both channels from one Pages project
(driven.maxhogan.dev/updates/stable/... and /updates/dev/...). If release.yml
deployed only `updates/stable/` it would wipe `updates/dev/`, and vice-versa.
Both manifest jobs therefore `pages deploy updates` (the whole tree), and
`scripts/generate-update-json.mjs` (M9a) MUST assemble the COMPLETE `updates/`
directory - the channel it is generating PLUS the other channel's current tree
(e.g. fetched from the live site or the GH-release-attached copies) - before the
deploy step runs. This is the one cross-file contract between M9d's workflows and
M9a's generator; it is called out here because getting it wrong silently breaks
the OTHER channel's updater on every publish.

### What remains to be proven at the M10 v0.1.0 tag

End-to-end is NOT exercisable on a push (no tag), so it is validated for real at
M10. Statically validated now: actionlint clean on all 5 workflows (0 findings),
JSON + YAML parse, ASCII + LF, secret names match what is provisioned, the
update.json path layout (`updates/<channel>/<target>/update.json`) matches M9a's
endpoint shape. Unproven until the tag: (1) tauri-action actually signs + uploads
all 4 targets and the .sig files; (2) `scripts/generate-update-json.mjs` exists
(M9a) and emits the full two-channel `updates/` tree; (3) wrangler-action
authenticates against account 9c20c14daa20466a2d761a47162f719a and the deployed
tree is reachable at driven.maxhogan.dev/updates/...; (4) release-please's Rust
strategy + the typed JSON extra-files bump all three version sources together;
(5) the dev `[dev-build]` / dispatch gate fires the matrix as intended; (6) the
tag-only chaos-real-drive leg passes with the live DRIVEN_E2E_* secrets.
## M9a - updater (in-app updater feature)

SHIPPED (SPEC s15, ROADMAP M9 part 1 - the IN-APP UPDATER feature only; the
release-pipeline GH Actions, CF hosting, telemetry, and pre-GA hardening remain
separate M9b/M9c/M9d workflows):

- `tauri.conf.json` `plugins.updater`: pubkey (the provisioned ed25519 public
  key), the STABLE default endpoint
  `https://driven.maxhogan.dev/updates/stable/{{target}}/{{current_version}}/update.json`,
  `dialog: false` (Driven shows its own banner). `{{channel}}` is deliberately NOT
  used (not a valid Tauri placeholder per SPEC s15.1) - the channel is in the PATH
  and chosen at runtime.
- Deps: `tauri-plugin-updater` + `tauri-plugin-process` (Cargo.toml), both plugins
  registered in `lib.rs`; `@tauri-apps/plugin-updater` + `@tauri-apps/plugin-process`
  (ui/package.json). Capabilities: `updater:default` + `process:default`.
- `src-tauri/src/updater.rs`: `Channel { Stable, Dev }` read/written via the SPEC
  s22 `updater.channel` settings group (no ad-hoc state; sibling fields preserved
  on write). `build_updater` overrides the runtime endpoint per channel via
  `app.updater_builder().endpoints(..)`. A periodic check (startup + every 6h via a
  `tokio::interval`, NOT a sleep/poll loop) `select!`s on a shutdown watch and is
  joined into the M5 quit drain (`AppState::shutdown_updater_task` + the bounded
  abort-capable `drain_restore_handle` in lib.rs) so quit leaves NO orphan. Emits
  `updater:available` / `updater:download_progress` / `updater:downloaded`. IPC:
  `check_for_update`, `install_update` (download_and_install + `app.restart()` via
  tauri-plugin-process), `get_update_channel`, `set_update_channel`.
- `scripts/generate-update-json.mjs` (Node ESM, pure, no network): writes per-target
  `update.json` into `updates/<channel>/<target>/<version>/update.json` matching the
  endpoint URL shape; derives version from tauri.conf.json (stable) or
  `0.0.0-dev.<sha>` (dev). Has `--help` + `--self-check`, unit-smoked from vitest.
- UI: `ChangelogModal.vue` (sanitized markdown release notes via
  `sanitizeMarkdown.ts` - HTML-escape-first + tag whitelist, XSS-safe; i18n).
  About tab extended with a channel toggle, Check-for-updates, an
  `updater:available` banner with Install + download progress + View-changelog, and
  paginated `list_releases`. `ui/src/stores/updater.ts` Pinia store + vitest.

TESTS (exercise the feature, no live endpoint): Rust - channel get/set round-trip
through settings, per-channel URL correctness (no `{{channel}}`, valid placeholders),
the available-update dispatch emits via a recording closure, up-to-date does not.
UI vitest - channel get/set, check available vs up-to-date -> banner, a live
`updater:available` event -> banner, install + download-progress fraction,
signature-failure error code, releases pagination, ChangelogModal render +
sanitizer XSS-safety. generate-update-json.mjs - shape + path-layout smoke against
a temp fixture.

DEVIATIONS / RESIDUALS:

- TEST SEAM: the plugin's real `check()` / `download_and_install` touch the network
  + filesystem and the `Update` struct cannot be constructed in a unit test, so the
  network-free DECISION logic (channel parse, URL build, `Update`->`UpdateInfo`
  mapping, available-update dispatch) is split into pure functions the tests
  exercise; `build_updater` / `run_check` / the real install are validated only by
  compilation + the manual M9 acceptance (a real CI release -> picks-up-update),
  NOT by an offline unit test (intentional - no test hits `driven.maxhogan.dev`).
- `UpdateInfo.published_at` for a checked manifest uses `OffsetDateTime::Display`
  (ISO-8601-shaped) rather than a strict RFC3339 format-description call, to avoid a
  direct `time` crate dependency; the UI parses it with `new Date(..)` and falls
  back to the raw string. The GitHub-releases path (M6 `check_for_updates`) is
  unchanged and still returns the API's RFC3339 `published_at`.
- The M6 `check_for_updates` / `list_releases` (GitHub releases API) commands are
  retained for the About tab's release-notes viewer; the new `check_for_update` /
  `install_update` (Tauri manifest) are the actual signed-update path. Both coexist
  by design (releases-API for notes, manifest for the staged signed download).
- The actual `update.json` hosting (`driven.maxhogan.dev/updates`) does not exist
  until the M9c CF-Pages workflow, and the GH Actions that CALL
  generate-update-json.mjs land in M9d - so end-to-end auto-update is not live yet;
  M9a delivers the in-app client + the manifest generator only.

## M9 fix round 1 (codex M9-1 xhigh: 7 P1 + 3 P2 + release-please config)

Source review: `.claude/codex-reviews/M9-1-20260624-202938.md` (baseline 97f596e, M9 @
3484889). All 10 findings were cross-track integration gaps from the concurrent
M9a (updater client) / M9d (release pipeline) split: neither track owned the
END-TO-END updater contract, so the endpoint layout, the manifest source, the
notes, the channel, and the dev metadata did not line up. One sole-actor fixed
them together (they are interdependent) plus the failing release-please config.

THE UNIFYING FIX - the updater PATH CONTRACT. Per Tauri's static-server model the
manifest is keyed by `{{target}}-{{arch}}` and carries the LATEST version (the
updater compares its running version to the manifest's), so the path must NOT
include the installed version. The single canonical layout, now byte-identical
across all five files:

    updates/<channel>/{{target}}/{{arch}}/update.json

(`{{target}}` = os: windows|darwin|linux; `{{arch}}` = x86_64|aarch64). The
`platforms` map KEY stays the combined `<os>-<arch>` (what Tauri matches at
runtime); only the directory layout splits it into <os>/<arch> segments. Files
kept in lockstep: `src-tauri/tauri.conf.json` endpoints, `src-tauri/src/updater.rs`
STABLE/DEV_ENDPOINT, `scripts/generate-update-json.mjs` (manifestOutPath +
osArchForTarget), `.github/workflows/release.yml`, `.github/workflows/dev-channel.yml`.

PER-FINDING:

- R1-P1-1 (drop {{current_version}}): removed the version segment from
  tauri.conf.json + updater.rs endpoints + the generator output path. updater.rs
  test now asserts NO `{{current_version}}`, HAS `{{arch}}`, ends `/update.json`.
  generate-update-json.mjs `manifestOutPath` dropped its `version` arg; new
  `osArchForTarget` splits the combined key into the <os>/<arch> dirs.
- R1-P1-2 (manifest job had no bundle artifacts): both publish jobs now
  `gh release download <tag>` the just-published installers + `.sig` into a flat
  `release-assets/` dir and pass `--assets-dir release-assets` to the generator
  (chose download-from-release over cross-job artifact upload - the assets already
  live on the release, so this is the smaller, single-source-of-truth path).
- R1-P1-3 (upload glob + RELEASE_TAG scoping): upload glob is now
  `updates/stable/**/update.json` (globstar) matching the final layout; the asset
  name flattens to `stable-<os>-<arch>-update.json`; `RELEASE_TAG` is a JOB-level
  env (`${{ github.ref_name }}`) so every step sees it.
- R1-P1-4 (dev generator missing --sha + wrong asset URL): dev publish job passes
  `--sha $(git rev-parse --short HEAD)` and `--base-url .../releases/download/dev`
  (the ROLLING dev tag; the generator also DEFAULTS dev's base-url to the `dev`
  tag now, since the bundle version is 0.0.0-dev.<sha> but the assets live on tag
  `dev`).
- R1-P1-5 (dev metadata never patched): new `scripts/set-dev-version.mjs` patches
  0.0.0-dev.<sha> into the THREE canonical sources release-please bumps - root
  Cargo.toml `[workspace.package].version` (src-tauri uses version.workspace=true),
  tauri.conf.json, ui/package.json - in a new "Patch dev version" step BEFORE the
  Tauri build, so the produced dev app actually reports the dev version.
- R1-P1-6 (empty notes): both publish jobs `gh release view --json body` into
  `release-notes.md` and pass `--notes-file`; the generator reads + trims it into
  the manifest `notes` (the in-app "View changelog" reads the manifest body).
- R1-P1-7 (CF deploy wipes the other channel): `pages deploy` is a whole-site
  snapshot, so each publish job runs new `scripts/fetch-live-channel.sh <other>`
  which curls the OTHER channel's currently-live per-platform manifests from
  `driven.maxhogan.dev/updates` and overlays them into the tree before deploy
  (never clobbering a locally-generated file; a 404/transient miss is skipped, not
  fatal). Chosen source-of-truth: the LIVE site itself (no extra branch/bucket).
- R1-P2-1 (macOS in-app install): new `ui/src/platform.ts` `isMacOS()` (userAgent,
  no new Tauri plugin/capability); About.vue gates the banner - macOS shows a
  "Download the latest release" link to /releases/latest + an unsupported note,
  Windows/Linux keep the in-app Install + progress. vitest covers both platforms.
- R1-P2-2 (pending consumed before install): `install_update` installs via
  `&update` and RESTORES the pending (with channel) on a failed
  download_and_install, so the banner's next Install retries. Pure
  `should_restore_pending(is_err)` unit-tested.
- R1-P2-3 (downloaded event hardcoded Stable): AppState `pending` now stores
  `(Update, channel_string)`; `install_update` emits `updater:downloaded` with the
  REAL channel via `downloaded_channel(channel_str)` (unit-tested: dev->dev,
  garbage->stable).

RELEASE-PLEASE CONFIG CHOICE. The first run failed `value at path package.version
is not tagged` because `release-type: "rust"` pointed at the VIRTUAL workspace
root `.` which has no literal `[package].version` (only `[workspace.package]` +
members using `version.workspace = true`). Switched to `release-type: "simple"`
(reads its version from `.release-please-manifest.json`, no Cargo `[package]`
parse) with `extra-files`: a `toml` updater for `Cargo.toml` jsonpath
`$.workspace.package.version` plus `json` updaters for tauri.conf.json +
ui/package.json `$.version`. This bumps the same three human-authored version
sources in lockstep without the virtual-manifest pitfall. NOTE: the `simple`
strategy does not rewrite Cargo.lock's per-crate `version =` lines; the workspace
is built WITHOUT `--locked`, so cargo reconciles the lock on the next build (no CI
break). The release tag stays `v<version>` (include-component-in-tag=false).

NEW TESTS: generate-update-json (osArchForTarget split; version-less os/arch path;
notes propagation; --notes-file + rolling-dev base URL); updater.rs
(downloaded_event_carries_the_real_channel; pending_update_survives_a_failed_install_only;
endpoint asserts no current_version + has arch); set-dev-version (workspace-version
edit touches only [workspace.package], JSON version set, version validation);
platform (isMacUserAgent); about-mac-gating (macOS hides install + shows DMG link,
Windows shows install). actionlint clean on all workflows.

RESIDUALS: the real `install_update` download path still cannot be unit-tested (the
plugin `Update` is not constructable offline) - the restore + channel POLICY is
unit-tested via the extracted pure helpers, but the live download is validated only
by compilation + the manual M9 acceptance. fetch-live-channel.sh's "download the
live tree" relies on the CF site being reachable at deploy time; a transient miss
briefly drops the other channel's manifest until its own workflow re-publishes
(strictly better than wiping AND failing the deploy).

## M9 fix round 2 (codex M9-2 recheck-1: 3 P1 + 2 P2)

Source review: `.claude/codex-reviews/M9-2-20260624-210939.md` (baseline 97f596e, M9 @
db60326). Recheck-1 of cap-2. Per-finding:

- R2-P1-1 (dev version below stable + non-monotonic). The dev build was
  `0.0.0-dev.<sha>`, which is LOWER than stable `0.1.0` (a stable user opting into
  `dev` was never offered an update) and short SHAs do not sort by time. FIX: derive
  `<next-patch>-dev.<run_number>.<short-sha>` from the CURRENT
  `[workspace.package].version` (NOT hardcoded). New pure helpers in set-dev-version.mjs:
  `readWorkspaceVersion`, `computeDevVersion(current, runNumber, sha)`,
  `computeDevVersionFromRepo`, plus a `--print-dev-version <run> <sha>` CLI mode. Both
  dev-channel.yml jobs (build + publish-manifest) compute the SAME value via that CLI
  (a pure function of the checked-out Cargo.toml + run_number + sha, so byte-identical),
  patch the app metadata with it, AND pass it to the generator via `--version`. SemVer
  ordering: 0.1.1-dev.* > 0.1.0 and run_number (numeric prerelease identifier) makes
  successive builds strictly increasing. Tests: computeDevVersion shape; dev > stable
  AND < next-stable; monotonic via run_number (sha is not the sort key); rejects a
  non-release base / bad run / bad sha (set-dev-version.test.ts, with an inline SemVer
  comparator proving the ordering).

- R2-P1-2 (rolling dev release accretes assets; generator silently keeps first
  duplicate -> manifest can point at a STALE signed bundle). FIX (both layers):
  (1) a new pre-build `clean-dev-assets` job wipes the rolling `dev` release's existing
  assets ONCE before the matrix build uploads the current run's bundles (a single job,
  not per matrix row, so the 4 parallel rows do not delete each other's fresh uploads;
  the release/tag is preserved, only assets cleared); (2) generate-update-json.mjs no
  longer silently keeps-first - `collectSignedBundles(dir, log, expectedVersion)` now
  groups by target and ERRORS on a stale bundle (a candidate whose filename version
  differs from the expected/manifest version) or any conflicting versions for one
  target; the legitimate single-build Windows `.msi`+NSIS `.exe` pair (same version) is
  kept deterministically (prefer NSIS) with a warning. Tests: versionFromBundleName;
  stale-old-version -> throws; same-version msi+nsis pair -> one manifest pointing at
  the NSIS installer (generate-update-json.test.ts).

- R2-P1-3 (`updater:available` lost if About not mounted). The only listener lived in
  About.vue (mounted on demand) but the backend STARTUP check emits early. FIX: own the
  updater event subscription at the APP ROOT (App.vue `onMounted` -> `updater.subscribe()`,
  never torn down - it is the app-lifetime component; About no longer subscribes/
  unsubscribes, it just reads shared store state). BELT-AND-SUSPENDERS: a new backend
  `get_pending_update_info` IPC command (peek, non-consuming) + `AppState::peek_pending_update`
  + the pure `pending_info_from_snapshot` mapper, with App.vue calling
  `updater.hydratePending()` on boot so an event that fired before the webview attached
  is still reflected. IPC contract kept in sync: ui/src/ipc/commands.ts `getPendingUpdateInfo`,
  lib.rs invoke_handler registration, store `hydratePending` action. Tests:
  pending_info_snapshot_maps_and_normalizes_channel (Rust); root-subscription event with
  no view mounted -> banner; hydratePending fills the banner; hydratePending does not
  clobber a fresher live update (updater-store.test.ts).

- R2-P2-1 (claimed wrong tauri-action input `uploadUpdaterJson` -> `includeUpdaterJson`).
  FALSE POSITIVE - NOT changed, and deliberately so. VERIFIED via TWO primary sources:
  (a) the authoritative tauri-action `action.yml` (dev branch) lists `uploadUpdaterJson`
  (default true) and has NO `includeUpdaterJson` input; (b) Context7's tauri-action docs
  agree (`uploadUpdaterJson`, default true). `includeUpdaterJson` does not exist and would
  be SILENTLY IGNORED, causing tauri-action to upload its own conflicting latest.json -
  the exact bug the review feared. So the current `uploadUpdaterJson: false` is correct;
  renaming it would BREAK the contract. Both workflows now carry an inline comment with
  the verification so a future reader does not "fix" it back. (Spec told us to verify via
  Context7 and not guess; the evidence overrides the finding.)

- R2-P2-2 (release notes hardcoded "See CHANGELOG.md for details."). FIX: a new
  unit-tested `scripts/extract-changelog.mjs` (pure `extractSection` / `headingVersion` /
  `normalizeVersion` over the release-please / Keep-a-Changelog heading shapes) extracts
  THIS tag's section. release.yml uses it for BOTH the GitHub Release `releaseBody` (build
  job, threaded via a GITHUB_OUTPUT heredoc) AND the manifest `--notes-file` (publish job,
  extracted directly from CHANGELOG.md, falling back to the GH release body only if the
  changelog has no section yet). `--allow-empty` + a bash fallback means a missing section
  never fails the release build. Tests: extract-changelog.test.ts (non-empty section for a
  tagged version, no bleed into adjacent sections, heading-shape parsing, empty for an
  unknown version).

Also done: removed the pre-existing unused `isX64` in generate-update-json.mjs.

GATES: cargo build --workspace --all-targets + clippy -D warnings + test --workspace
(151+197+43... all green, incl. the new updater test) + build -p driven-app + deny check
(ok) + fmt --check + git diff --check all clean. ui: pnpm install (lockfile unchanged) +
lint + test:unit (130 passed) + build (vue-tsc --noEmit clean). actionlint 0 findings on
all workflows. No sqlx::query! touched (the peek reads an in-memory mutex), so no
.sqlx regen. Anti-fake-green stub sweep on src-tauri/src + scripts + ui: zero non-test
todo!/unimplemented!/unreachable!.

RESIDUALS: same `install_update` live-download untestability as round 1 (plugin Update
not constructable offline). The byte-identical-dev-version guarantee rests on both
dev-channel.yml jobs running `--print-dev-version` against the SAME checked-out commit
with the same run_number + sha - true within one workflow run by construction (concurrency
group serializes runs; the publish job `needs: build`).

## M9 fix round 3 (codex M9-3 recheck-2: 2 P1 + 1 P2) - release-pipeline integrity, HARD STOP

Source review: `.claude/codex-reviews/M9-3-20260624-214234.md` (baseline 97f596e, M9 @
5e877ce). Scope was the release pipeline ONLY (generate-update-json.mjs + release.yml +
dev-channel.yml + their tests); no Rust/Vue product code changed. tauri-action's macOS
`.app.tar.gz` naming + the `releaseAssetNamePattern` `[arch]` behavior were confirmed via
Context7 (`/tauri-apps/tauri-action`), not guessed: the on-disk macOS updater artifact is
named from the `.app` bundle and carries the VERSION but NO arch, so both mac matrix rows
emit the same basename; `releaseAssetNamePattern: '[name]_[version]_[platform]_[arch][ext]'`
forces the arch into the uploaded asset (and its `.sig`) at tauri-action's release-upload
step. Per-finding:

- R3-P1-1 (macOS arch collision; archless mac bundle silently classified as x86_64). Both
  `aarch64-apple-darwin` and `x86_64-apple-darwin` jobs produced an archless
  `Driven.app.tar.gz`, so in a FLAT GitHub Release asset set one arch was lost or ARM was
  advertised as x86_64; `targetForBundle()` defaulted an archless mac bundle to
  `darwin-x86_64`. FIX (consistent naming across BOTH workflows + the generator parser):
  (1) release.yml tauri-action now sets `releaseAssetNamePattern:
  "[name]_[version]_[platform]_[arch][ext]"` so every uploaded asset (incl. each mac
  `.app.tar.gz` + `.sig`) carries the arch; (2) dev-channel.yml's `Collect bundle
  artifacts` step - which uploads on-disk bundles itself (tauri-action only builds there,
  no tagName, so the pattern cannot apply) - now stamps the matrix arch into the mac
  `.app.tar.gz`/`.sig` name (`Driven_<arch>.app.tar.gz`), idempotently (skips names that
  already carry an arch token); (3) `targetForBundle()` now REJECTS (throws) an archless
  mac updater bundle instead of guessing x86_64, and recognizes the x86_64 token set
  (`x86_64|x64|amd64|intel`). The throw propagates out of `collectSignedBundles` (a non-mac
  unmapped file like `.deb` still returns null and is skipped). Tests: arch-named forms map
  to darwin/aarch64 + darwin/x86_64; an archless `Driven.app.tar.gz` throws from both
  `targetForBundle` AND a full `generate` run; two arch-named mac bundles yield BOTH
  darwin/x86_64 AND darwin/aarch64 manifests with no platform-key collision.

- R3-P1-2 (partial updater trees go green). The generator only errored on ZERO bundles and
  the workflows only checked the manifest glob was non-empty, so a missing `.sig`, an asset
  collision, or a mapping miss could deploy a PARTIAL tree while CI passed. FIX: a new
  `--require-targets <comma/space-list>` generator option (parsed + validated by
  `parseRequiredTargets`, checked by `assertRequiredTargets`) ERRORS unless EVERY named
  combined `<os>-<arch>` key produced a manifest. Both workflows pass the EXACT V1 set
  `windows-x86_64,darwin-x86_64,darwin-aarch64,linux-x86_64` (also exported as
  `V1_REQUIRED_TARGETS`). Defense-in-depth: each workflow ALSO has a shell step asserting
  every required `updates/<channel>/<os>/<arch>/update.json` exists before any upload/deploy.
  Tests: assertRequiredTargets passes on the full set + names the gap on a missing one;
  parseRequiredTargets dedupes/validates/rejects empty+malformed; `generate` with
  `requireTargets` FAILS on a Windows-only dir and PASSES on the full four-target dir.

- R3-P2-1 (stale `--sha` dev-version: `0.0.0-dev.<sha>`, contradicting set-dev-version.mjs).
  The generator re-implemented a below-stable dev version, a SECOND source of truth. FIX:
  removed `--sha` and the `0.0.0-dev.<sha>` default entirely. The generator now statically
  imports `computeDevVersionFromRepo` + `isValidVersion` from set-dev-version.mjs (the ONE
  source of truth). `resolveVersion` for dev REQUIRES `--version` (the workflow's normal
  path - it threads the shared `--print-dev-version` value) or, for a manual run,
  `--run-number <n> --dev-sha <sha>` which DELEGATES to the shared monotonic helper; it
  never derives a contradictory version. `computeDev` is injectable for tests. Help + header
  comments + the stale test rewritten. Tests: explicit `--version` used verbatim; dev
  delegates to the injected shared computeDev (and is ABOVE stable, never 0.0.0); dev with
  neither `--version` nor run/sha is rejected; invalid `--version` rejected.

GATES: cargo build --workspace --all-targets (SQLX_OFFLINE) + clippy -D warnings + test
--workspace + build -p driven-app + deny check + fmt --check + git diff --check; ui pnpm
install + lint + test:unit + build (vue-tsc --noEmit clean); actionlint 0 findings on all
workflows. No sqlx::query! touched (no .sqlx regen). Anti-fake-green stub sweep on scripts
+ src-tauri/src + ui: zero non-test todo!/unimplemented!/unreachable!.

FINAL VALIDATION + HARD STOP: the release pipeline's TRUE test is the real M10 v0.1.0 tag
run (release.yml fires only on a `v*` tag; dev-channel on `[dev-build]`/dispatch) - a static
generator/workflow review cannot exercise tauri-action's real per-OS bundle output, the
flat-release asset set, or the CF Pages whole-site snapshot. Per the round-3 spec, the
release-pipeline codex loop HARD-STOPS after recheck-3: any further pipeline-only edges are
to be validated/hardened against the actual M10 tag run, NOT chased as static hypotheticals
indefinitely.

## M9 fix round 4b (codex M9-4: 2 release-pipeline P1 + 1 updater-store P2; r4b worktree, concurrent with r4a)

Source review: `.claude/codex-reviews/M9-4-20260624-223910.md` (baseline 97f596e, M9 @ f9b1a41).
This round split: the 2 DATA-SAFETY P1s (recovery durability + update_source ack gate) were r4a
(a concurrent main-checkout agent on disjoint Rust/SourceTable files); r4b (this worktree) owns the
2 release-pipeline P1s + the updater-store P2. Files touched here:
`.github/workflows/dev-channel.yml`, `scripts/fetch-live-channel.sh`, `ui/src/stores/updater.ts`,
`ui/src/App.vue`, `ui/src/__tests__/updater-store.test.ts` (release.yml already routes through the
shared fetch script, so its deploy path is covered transitively).

### R4-P1-3 (dev-channel.yml) STAGE-THEN-PUBLISH - never delete dev assets before the rebuild validates
Before: a `clean-dev-assets` job deleted ALL of the rolling `dev` release's assets BEFORE this run's
matrix built/uploaded. A failed/cancelled run after that delete (and `concurrency.cancel-in-progress:
true` makes cancellation routine) left the live CF Pages dev manifests pointing at now-deleted GitHub
assets -> broken dev auto-update until the next good run.
Fix: removed the pre-build delete job. New ordering is build -> publish-dev-manifest -> gc-stale-dev-assets:
  1. The build job stamps THIS run's run-unique dev version (`<next-patch>-dev.<run>.<sha>`) into
     EVERY collected asset name - including the macOS `.app.tar.gz` (which previously carried neither
     arch nor version), so no asset can collide with a prior run's on the rolling release.
  2. publish-dev-manifest downloads ONLY the assets whose filename contains this run's dev version
     (pure substring match), so the generator's stale-bundle guard sees a single clean run. It then
     generates + validates all 4 target manifests, overlays the live stable channel (fail-closed,
     below), and deploys the whole-site snapshot. A failure ANYWHERE up to here leaves the previous
     good dev release (assets AND live manifests) fully intact.
  3. gc-stale-dev-assets runs ONLY on `needs.publish-dev-manifest.result == 'success'` and deletes
     every rolling-release asset NOT carrying this run's dev version. A failure of THIS job alone is
     harmless (stale assets linger until the next dev build GCs them).
So there is no longer any delete-then-rebuild window; cancellation is now safe.

### R4-P1-4 (fetch-live-channel.sh, used by BOTH release.yml + dev-channel.yml) FAIL CLOSED
Before: the overlay script treated transport/non-200 fetches of the OTHER channel's live manifests as
"skip". Because the subsequent `pages deploy` is a WHOLE-SITE snapshot, a transient blip while
preserving e.g. `stable` during a `dev` deploy dropped stable's manifests from the snapshot and the
deploy WIPED them -> every stable user's auto-update broke.
Fix: fail closed. Each per-platform fetch now retries (curl `--retry 3 --retry-all-errors` inside an
outer 4-attempt loop with backoff + connect/max timeouts). A genuine HTTP 404 is the ONLY tolerated
miss (the known first-publish case - that channel/platform was never published, nothing to preserve).
ANY persistent non-404 outcome (transport, timeout, 5xx, 403, 200-with-empty-body) makes the script
exit non-zero, which fails the workflow step and ABORTS the deploy before the whole-site snapshot can
wipe the live channel. The script lists EVERY failing manifest before aborting. Applies to both deploy
paths because both invoke this one script.
Durable-source-of-truth option (future, removes the fetch dependency entirely): commit each channel's
published manifests to the repo (or keep a canonical R2/KV copy) and deploy the MERGE of both channels
from there, so a whole-site deploy never depends on re-fetching the other channel's live tree. Until
then, fail-closed is the safe behavior.

### R4-P2-1 (ui/src/stores/updater.ts + App.vue) subscribe() partial-failure cleanup + hydration ordering
Before: `subscribe()` set `desiredSubscribed=true`, then `Promise.all`'d the three listener
registrations. If one rejected, the other (already-resolved) listeners stayed attached but their
unlisten handles were never assigned (leaked), and `desiredSubscribed` stayed `true` so every later
`subscribe()` no-opped forever - the store could never recover, and an `updater:available` event
could fire into a dead store. App.vue also `await`ed subscribe() before hydration, so a subscribe
rejection skipped `get_pending_update_info` hydration entirely.
Fix: subscribe() now uses `Promise.allSettled`, collects the handles that DID register, and on ANY
rejection (or an `unsubscribe()` race) unlistens everything that registered, nulls the handles, resets
`desiredSubscribed=false`, and re-throws so a retry can re-subscribe. Only on full success are the
handles kept. App.vue wraps `subscribe()` in try/catch and runs `hydratePending()` in a `finally`, so
a subscribe failure logs but never skips pending-update hydration (an independent path that still
surfaces the banner with no live listeners). New vitest asserts: a partial subscribe failure leaks no
listener (only the registered ones are torn down, a would-be-leaked event does not mutate the store),
a later subscribe succeeds (desiredSubscribed was reset) and its live event surfaces the banner, and
hydration still runs.

## M9 fix round 4a

Codex M9-4 (baseline 97f596e, M9 @ f9b1a41) raised 4 P1 + 1 P2. Round 4a fixes the
TWO DATA-SAFETY P1s (recovery-phrase gate DURABILITY + the update_source enable gate);
the other two P1s (dev-channel stage-then-publish, fetch-live fail-closed) + the
updater-store P2 are round 4b (a concurrent worktree agent on .github/ + scripts +
updater store). Round 4a touched ONLY src-tauri/src (recovery/sources/app_state/lib +
dtos) + crates/driven-core (state trait/sqlite + a new migration) + ui (SourceTable.vue,
types.ts, locales, test fixtures). All gates green on Windows: cargo build/clippy
-Dwarnings/test --workspace (159 driven-core + 49 driven-app, all pass) + build -p
driven-app + deny check + fmt --check + git diff --check; just sqlx-prepare (6 new .sqlx
entries, 0 drift); cd ui pnpm install/lint/test:unit (144 pass)/build (vue-tsc clean).
Anti-fake-green stub sweep on the touched surface (src-tauri/src + ui): zero non-test
todo!/unimplemented!/unreachable!.

- **R4-P1-1 - recovery-phrase ACK gate made DURABLE [DATA-SAFETY] FIXED.** M9c D4
  persisted the FIRST encrypted source `enabled:false` + master key, but the pending-ack
  gate state (pending + revealed) lived ONLY in process memory (app_state `recovery_acks`).
  A crash AFTER the source + master key were persisted but BEFORE reveal+ack lost the
  gate: the user could no longer reveal/ack the original phrase, and later encrypted
  sources could arm encryption without it (unrestorable backups). Fix: a new migration
  `0004_recovery_phrase_acks.sql` adds a `recovery_phrase_acks` table (source_id PK + FK
  ON DELETE CASCADE, account_id, revealed, created_at). New StateRepo methods (SQLite
  transactional, default impls for in-memory doubles):
  `insert_first_encrypted_source_pending_ack` writes the master-key CAS stamp + the
  (disabled) source insert + the pending-ack record ALL in ONE transaction (a durable
  encrypted source can never exist without its durable pending-ack record);
  `list_pending_recovery_acks` (startup reconstruction); `mark_recovery_phrase_revealed`
  (durable revealed=1); `recovery_ack_revealed` (the gate predicate); and
  `enable_source_and_clear_recovery_ack` (atomic enable + delete-record on ack). The
  command layer now reads/writes the DURABLE table for every gate decision: `add_source`
  uses the atomic insert, `reveal_recovery_phrase` resolves the owning account from the
  durable record + durably records the reveal, and `ack_recovery_phrase_saved` checks the
  durable `revealed` flag then commits the atomic enable+clear. `AppState` keeps the
  in-memory `recovery_acks` as a reconstructed-on-startup MIRROR
  (`reconstruct_recovery_acks_from_db`, called in lib.rs setup after build_and_spawn).
  Invariant: after ANY restart a not-yet-acked encrypted source is still disabled + still
  reveal/ackable, and no second encrypted source can enable without the durable ack. Tests
  (driven-core sqlite): `first_encrypted_source_pending_ack_persists_atomically`,
  `..._rolls_back_on_failure`, `recovery_ack_gate_survives_restart_then_durable_ack_enables`
  (reload over the SAME db file -> still disabled + ackable -> durable ack flips it),
  `mark_revealed_and_revealed_query_unknown_source_are_noops`,
  `delete_source_cascades_its_pending_recovery_ack`; (driven-app)
  `find_pending_recovery_ack_account_resolves_durable_record`.
- **R4-P1-2 - update_source must enforce the ack gate [DATA-SAFETY] FIXED.**
  `update_source` (and any enable/sync-trigger path that toggles `enabled`) could flip a
  disabled pending encrypted source to `enabled=true`, bypassing the gate. Fix: a shared
  `reject_enable_of_pending_encrypted_source` helper REJECTS enabling a source that still
  has a durable `recovery_phrase_acks` record (the s24 `internal.invalid_input` code);
  `ack_recovery_phrase_saved` remains the ONLY enable path for a pending source. The
  durable record is the source of truth so this holds across a restart. UI:
  `SourceDto.pendingRecoveryAck` (enriched in `list_sources` + returned by `add_source`)
  disables the SourceTable enable toggle with a tooltip + an "Awaiting recovery phrase"
  badge (new i18n keys `settings.sources.pendingRecoveryAck{Badge,Tooltip}`); the backend
  rejection is the real guard. Tests: (driven-app)
  `reject_enable_of_pending_encrypted_source_gate` (rejected until the durable ack, allowed
  after); (vitest) `disables the enable toggle for a pending-recovery-ack source (R4-P1-2)`.

## M9 fix round 5b (codex M9-5: updater pubkey GA-blocker + pending hygiene; r5b worktree, concurrent with r5a)

Scope: the two updater findings from codex M9-5 (R5-P1-4 pubkey format, R5-P2-1 pending
hygiene). The 3 data-safety P1s in the same review (remove-pending-encrypted-source orphan,
post-restart reveal/ack action, restore root TOCTOU) are r5a's. r5b touched ONLY
`src-tauri/src/updater.rs`, `src-tauri/tauri.conf.json` (unchanged in the end - see verdict),
`src-tauri/Cargo.toml` (dev-deps), and a NEW committed smoke
(`src-tauri/tests/release_signature_smoke.rs` + `tests/fixtures/updater/fixture.bin` + `.sig`).

- **R5-P1-4 - updater pubkey format [GA-BLOCKER] - SETTLED EMPIRICALLY: codex was WRONG, pubkey
  KEPT.** Codex claimed the configured `pubkey` (base64 of the whole minisign `.pub` file,
  `dW50cnVzdGVkIGNvbW1lbnQ6...`) was wrong and that Tauri wants the bare `RWS...` line. This was
  a FALSE POSITIVE; changing it would have BROKEN production update verification. Proof, read out
  of the in-tree `tauri-plugin-updater` 2.10.1 source (`updater.rs:1453` `verify_signature`):
  it does `let pub_key_decoded = base64_to_string(pub_key)?; PublicKey::decode(&pub_key_decoded)?`
  i.e. it base64-DECODES the configured `pubkey` FIRST, then parses the decoded minisign text
  (`untrusted comment: ...\nRWS...`). So the config MUST be the base64-of-the-.pub-file form; the
  bare `RWS...` line would fail `base64_to_string`/`PublicKey::decode` and break every update.
  EMPIRICAL settle: signed a throwaway fixture with the real updater private key
  (`cargo tauri signer sign`, key at `~/.tauri/driven-updater.key`, NEVER printed/committed),
  committed only the fixture + its `.sig`, and verify them against the tauri.conf.json `pubkey`
  via the EXACT same decode+verify path tauri uses (base64-decode pubkey -> `PublicKey::decode`;
  base64-decode `.sig` -> `Signature::decode`; `verify(data, &sig, allow_legacy = true)`). The
  configured pubkey VERIFIES the signature -> KEPT as-is. Regression smoke
  `release_signature_smoke.rs` (3 tests): `configured_pubkey_verifies_the_committed_fixture_signature`
  (the GA guard - reads the pubkey LIVE from tauri.conf.json so a wrong edit / key drift fails CI),
  `configured_pubkey_is_base64_of_a_minisign_pub_file_not_the_bare_rws_line` (asserts the format and
  that the bare RWS line FAILS verify - locks the verdict), `tampered_fixture_fails_verification`
  (one-byte flip fails - proves the smoke really checks the sig). VERIFY needs no private key, so CI
  runs it every build via `cargo test --workspace`. minisign-verify 0.2.5 + base64 (workspace) added
  as DEV-deps only; both already in the graph transitively via tauri-plugin-updater, so NO new crate
  and deny check is unaffected. NOTE: the test FILE was renamed off `updater_signature` to
  `release_signature_smoke` because a test-binary name containing "update" trips Windows UAC
  installer-detection (`os error 740: requires elevation`) and would fail headless CI.
- **R5-P2-1 - clear stale pending update on UpToDate / channel switch FIXED.**
  `periodic_check_once` previously only SET pending when a check found an update; an `UpToDate`
  result left a prior pending `Update` intact, and `set_update_channel` persisted the new channel
  without clearing the backend pending update - so `get_pending_update_info` / `install_update`
  could refer to an already-installed or old-channel update. Fix: `periodic_check_once` now CLEARS
  pending on UpToDate (and still sets on found), and `set_update_channel` clears pending after a
  successful channel write. The clear-vs-keep policy is routed through pure unit-tested predicates
  (`pending_action_for_check`, `pending_action_for_channel_switch` -> `PendingAction`), mirroring the
  existing `should_restore_pending` / `dispatch_check_outcome` test seam (a real
  `tauri_plugin_updater::Update` can't be constructed in a unit test, so the decision is tested pure
  and the side-effecting `set_pending_update(None)` rides the branch). Tests:
  `up_to_date_check_clears_pending_available_update_sets`, `channel_switch_always_clears_pending`.
  (`set_pending_update(None)` is a pre-existing AppState method - app_state.rs was NOT modified.)

Gates (worktree, base cf8d5d3): cargo build --workspace --all-targets + clippy
--workspace --all-targets -D warnings + test --workspace (incl. the new smoke; 27 ok groups,
0 fail) + deny check (advisories/bans/licenses/sources ok) + fmt --check + git diff --check all
GREEN. ui: pnpm install/lint/test:unit (145)/build (vue-tsc) all GREEN. Stub sweep: zero
non-test todo!/unimplemented!/unreachable! on src-tauri/src. No CI workflow added (smoke runs
under cargo test), so actionlint N/A.

## M9 fix round 5a (codex M9-5: 3 DATA-SAFETY P1s; CONCURRENT with r5b)

Baseline cf8d5d3. Disjoint from r5b (updater.rs / tauri.conf.json). Touched only
src-tauri/src/commands/{sources,restore}.rs, crates/driven-core/src/state/{mod,sqlite}.rs,
ui/src/components/SourceTable.vue (+ en-US.json + a vitest). All gates green incl. vue-tsc +
sqlx 0-drift; non-test stub sweep on the touched surface = 0.

- **R5-P1-1 - removing a PENDING encrypted source orphaned the master key [DATA-SAFETY] FIXED.**
  `remove_source` on a first encrypted source the user never acked dropped `backup_sources`
  (cascading the `recovery_phrase_acks` row) but LEFT `accounts.encryption_master_key_id` +
  the keychain master key. The next encrypted source then took the "already provisioned"
  path (no recovery phrase) and could arm encryption the user can never restore. Fix:
  `remove_source` now detects a durable pending ack (`recovery_ack_revealed(id).is_some()`)
  and routes through a NEW atomic discard transaction
  `StateRepo::discard_pending_encrypted_source` (default impl + real SQLite `BEGIN/COMMIT`
  override): it deletes the source (cascading the ack + file_state + pending_ops) AND, ONLY
  when it was the account's SOLE encrypted source, clears the account master-key stamp,
  returning `DiscardPendingOutcome { master_key_cleared, account_id }`. When
  `master_key_cleared`, the command then deletes the keychain master key (idempotent; never
  deleted while another encrypted source still needs it). Net invariant: a later encrypted
  source ALWAYS either reuses an acked phrase or triggers a FRESH reveal+ack - never the
  silent already-provisioned path. Tests (driven-core sqlite):
  `discard_pending_only_encrypted_source_clears_master_key_stamp`,
  `discard_pending_keeps_master_key_when_another_encrypted_source_exists`,
  `discard_pending_unknown_source_is_a_noop`; (driven-app)
  `discard_pending_encrypted_source_clears_stamp_then_next_add_reveals_fresh_phrase`.

- **R5-P1-2 - no post-restart reveal/ack UI [DATA-SAFETY] FIXED.**
  After a crash/restart before ack, the durable pending source showed only a disabled
  toggle + badge; the reveal/ack flow lived only in volatile wizard state, so r4a's
  "still reveal/ackable after restart" invariant was unreachable. Fix: SourceTable now has
  a pending-recovery ROW ACTION ("Reveal and save recovery phrase") that calls
  `revealRecoveryPhrase(id)` (records the durable backend reveal + returns the 24 words),
  shows them via the existing `RecoveryPhraseReveal` gated on the user attesting they saved
  them, then `ackRecoveryPhrase(id)` (the backend's ONLY enable path, rejected without a
  recorded reveal) which enables the source + clears pending, and refreshes. Remove of a
  pending source is no longer dangerous (it is the safe discard from R5-P1-1); the remove
  confirm panel shows a clear "discards the encrypted setup" warning for a pending row. New
  i18n keys `settings.sources.{revealAckButton,revealAckIntro,revealAckConfirmButton,
  pendingRemoveWarning}`. Test (vitest): `exposes a post-restart reveal/ack action that
  enables a pending source (R5-P1-2)`.

- **R5-P1-3 - restore ROOT confinement was a re-resolved path STRING (TOCTOU) [DATA-SAFETY] FIXED.**
  `restore_files` built `DialogToken::for_root(dest_dir)` and `ConfinedDest::open`
  re-canonicalised that STRING on every file. The per-component no-follow parent walk pinned
  components BELOW the root, but the ROOT itself was still re-resolved - a root swapped to a
  symlink/junction between bind and a later open redirected decrypted bytes outside the
  user-chosen directory. Fix: bind a STABLE root IDENTITY once at consume time via the new
  `confine::ConfinedRoot::bind` (canonical path + on-disk identity: Unix `(st_dev, st_ino)`,
  Windows `(dwVolumeSerialNumber, nFileIndex{High,Low})` from a no-follow
  `BY_HANDLE_FILE_INFORMATION`; cfg-gated, `None` on unsupported targets falls back to the
  canonical-path equality). The background job carries the `ConfinedRoot`, and
  `ConfinedDest::open` re-verifies (`ConfinedRoot::verify`) the re-resolved root matches the
  bound canonical path AND identity, REJECTING (`local.io_error`) before any handle/temp is
  opened on a mismatch. `validate_restore_dest` still runs off `ConfinedRoot::token()` for
  the structural dir-chain pre-step. Residual: the per-file open's parent walk already pins
  components below the root no-follow (R2-P1-1); the new identity bind closes the
  root-level analogue. Test (unix): `confined_open_rejects_root_swapped_to_symlink_after_bind`
  (root swapped to a symlink-out-of-root AFTER bind is rejected, no bytes land outside the
  originally-bound root); the Windows `RootIdentity` capture/verify path is exercised by the
  existing Windows confine tests (they now bind a `ConfinedRoot`).

## M9 fix round 6

Codex M9-6 (baseline 97f596e, M9 @ 74c98dd): 2 P1 + 1 P2, all legitimate, single sole-actor.
Touched `src-tauri/src/commands/sources.rs`, `scripts/generate-update-json.mjs`,
`ui/src/__tests__/generate-update-json.test.ts`, `src-tauri/src/updater.rs`,
`src-tauri/src/tray.rs`, `src-tauri/locales/en-US.yml`. All gates green incl. clippy -D warnings,
fmt, deny, vue-tsc; no sqlx/migration/workflow change (no sqlx-prepare / actionlint needed);
non-test stub sweep on the touched surface = 0.

- **R6-P1-1 - second encrypted add bypasses the recovery-phrase ack gate [DATA-SAFETY] FIXED.**
  The ack gate only triggered when THIS `add_source` call newly generated the account master
  key (`newly_generated_key`). A SECOND encrypted `add_source` while the FIRST encrypted source
  was still pending its ack saw the already-stamped account, got `newly_generated = false`,
  persisted `enabled: true`, inserted with NO pending-ack record, and reconfigured the
  orchestrator - so encrypted backups for the second source began BEFORE any recovery phrase
  was ever revealed/acked (unrestorable on a new machine). Fix: inside the per-account
  encrypted-source lock (so the read is current and serialised against a racing add), BEFORE
  preparing the master key, `reject_encrypted_add_while_ack_pending` reads the DURABLE
  `recovery_phrase_acks` table and REJECTS the encrypted add (stable s24 `internal.invalid_input`,
  message guiding the user to finish revealing+saving the pending source's phrase first) if ANY
  row exists for the account. Chose the REJECT over the disabled-then-enable path (spec's stated
  preference - simpler + unambiguous; the disabled path would also leak a second pending-ack
  record with no second-source reveal flow). Net invariant: no encrypted source on an account can
  be enabled / start backups while any recovery-phrase ack is still pending on that account.
  Test (driven-app): `reject_encrypted_add_while_ack_pending_blocks_second_encrypted_add` - with
  account A's first encrypted source seeded pending+disabled, a second encrypted add on A is
  rejected (InvalidInput) and the first source stays DISABLED (no backups) through the pending
  window; a different account B is unaffected; after the durable ack clears, a further encrypted
  add on A is allowed.

- **R6-P1-2 - orphan .sig produces a broken update.json [PIPELINE INTEGRITY] FIXED.**
  `collectSignedBundles` trusted a `.sig` file as proof its sibling installer existed: it derived
  `bundleFile` from the `.sig` path, read ONLY the signature, and emitted a download URL from the
  bundle name. A partial release (`Driven...exe.sig` but no `Driven...exe`) still produced a
  valid-looking manifest whose `url` 404s, so clients discover an update they cannot download.
  Fix: before accepting a candidate, `await fs.stat(bundleFile)` and require it to be a regular
  FILE - an orphan `.sig` (stat fails) or a non-file at that path ERRORS the run with a clear
  message ("orphan signature ... no sibling installer ... R6-P1-2"), so a partial update tree
  never publishes. Tests (vitest): an orphan `.sig` makes `generate` reject AND writes no
  manifest; `collectSignedBundles` rejects the orphan but accepts once the real installer is
  dropped next to it.

- **R6-P2-1 - dev-channel periodic checks did not apply silently [SPEC-CONFORMANCE] FIXED.**
  DESIGN s9.4 ("New dev -> applies silently with a tray notification"), but the periodic check
  stored a pending update + emitted `updater:available` (the manual banner path) for ALL channels.
  Fix: a pure per-channel router `delivery_for_periodic_available(channel)` -> `SilentInstall`
  for `Dev`, `ManualBanner` for `Stable`. `periodic_check_once` branches on it: STABLE keeps the
  existing manual banner (record pending + emit available); DEV calls
  `silent_install_dev_update`, which reuses the SAME `download_and_install` plumbing as manual
  `install_update` (emitting `updater:download_progress` + the channel-correct `updater:downloaded`
  so any open UI still reflects progress), then raises a tray notification via the new
  `tray::notify_dev_update_installed` (new i18n keys `notifications.dev_update_installed.{title,
  body}`). A dev download/verify failure is logged + swallowed (the periodic loop survives; the
  next interval retries) and is NOT surfaced as a banner (dev is the silent channel). Test
  (driven-app): `dev_channel_periodic_available_takes_silent_install_path`.

  **DEV-UPDATE UX INTERPRETATION (DESIGN s9.4 is terse - documenting the chosen faithful reading):**
  "applies silently with a tray notification" is implemented as DOWNLOAD + INSTALL (stage the
  update via `download_and_install`) + a TRAY NOTIFICATION, with the staged update applying on the
  NEXT Driven restart - we deliberately do NOT call `app.restart()` to force an immediate relaunch.
  Rationale (spec's instruction: if ambiguous, implement the safest faithful reading - download+
  install, tray-notify, apply on next restart): "silently" means no banner / no user-gated install
  prompt (the install happens with no interaction), not "yank the app out from under a power user
  mid-work". Forcing an immediate restart would terminate in-flight backups + lose unsaved UI
  state with no consent - more disruptive than the stable banner it is meant to be lighter than.
  The tray toast tells the power user a fresh dev build is staged and will apply on restart. This
  matches Tauri's native staged-update model (install stages; the new binary is picked up on next
  launch) and keeps the manual `install_update` (which DOES `app.restart()`) as the only forced-
  relaunch path. If the product later wants an immediate auto-restart for dev, it is a one-line
  change (call `app.restart()` after the tray notify) - the plumbing is already wired.

## M9 fix round 7b

Source: codex review `.claude/codex-reviews/M9-7-20260625-002821.md` (baseline 97f596e, M9 @
23b15d3). This round fixes the single GA-blocker pipeline finding P1-1 (the 2 data-safety findings
are round 7a, concurrent). Files touched: `.github/workflows/release.yml`,
`.github/workflows/dev-channel.yml`, `scripts/fetch-live-channel.sh` (header docs only - the script
logic is tree-dir-parameterized and unchanged). updater.rs and tauri.conf.json were NOT touched
(option (a) chosen - see below).

### R7-P1-1 (release.yml + dev-channel.yml + updater.rs:79) deploy path != fetch path -> 404
The in-app updater fetches `https://driven.maxhogan.dev/updates/<channel>/{{target}}/{{arch}}/update.json`
(updater.rs `STABLE_ENDPOINT`/`DEV_ENDPOINT`, tauri.conf.json plugins.updater.endpoints). Both
manifest jobs previously ran `wrangler pages deploy updates`. Per Cloudflare Pages semantics
(verified via Context7 cloudflare-docs: `wrangler pages deploy <dir>` uploads `<dir>`'s CONTENTS as
the deployment ROOT - subfolders map 1:1 to URL path segments), deploying the bare `updates/` dir
served the manifests at `/<channel>/...` (the `/updates/` prefix STRIPPED), so every stable + dev
updater check 404'd. GA-blocker.

**Option chosen: (a) - deploy a `site/` STAGING PARENT that contains `updates/`.** Strongly
preferred over (b) because it keeps updater.rs + tauri.conf.json + generate-update-json.mjs
BYTE-IDENTICAL (zero runtime-endpoint surface change) and leaves the Pages site ROOT free for the
future landing page (#12). Concretely, both jobs now:
  - generate into `site/updates/<channel>/...` via the generator's existing `--out site/updates`
    flag (the generator itself is unchanged - `--out` already existed; default stays `./updates`);
  - overlay the OTHER channel's live manifests into the same staging tree by passing tree-dir
    `site/updates` to `scripts/fetch-live-channel.sh` (the script appends
    `<channel>/<plat>/update.json` to whatever tree-dir it is given, so it works unchanged for
    `updates` or `site/updates`);
  - (release.yml only) glob the GitHub-Release manifest upload + the required-manifest existence
    check from `site/updates/stable/**` instead of `updates/stable/**`;
  - run `wrangler pages deploy site` (the PARENT) instead of `pages deploy updates`. Pages now
    serves `site/`'s contents at root -> `/updates/<channel>/<os>/<arch>/update.json`, matching the
    app's fetch endpoint EXACTLY. No updater.rs/tauri.conf.json edit was needed.

**Post-deploy curl smoke (NEW, in BOTH workflows).** After the `pages deploy site` step, each job
curls all four REQUIRED targets' `update.json` at the REAL public URL the app fetches
(`$UPDATES_BASE/<channel>/<os>/<arch>/update.json`) and FAILS the job (non-zero exit) unless every
one returns HTTP 200 AND a JSON body with a non-empty `version` string. This is what catches the
deploy-path != fetch-path mismatch FOR REAL at release/dev-build time - if the prefix is ever wrong
again, or a manifest is missing, the smoke 404s and the job fails instead of silently shipping a
broken channel. The curl uses a bounded `--retry` (NOT a sleep/poll loop) to ride out the few-second
Cloudflare Pages propagation lag. (Caveat: real end-to-end CF serving is only exercised on a tagged
release / dev-build run that actually deploys; the smoke guards the path at that point. Plain CI / PR
runs do not deploy, so they cannot exercise the live curl - that is by design.)

## M9 fix round 7a (codex M9-7: data-safety P1-2 + P2-1)

Source review: `.claude/codex-reviews/M9-7-20260625-002821.md` (baseline 97f596e, M9 @ 23b15d3).
This round is the two DATA-SAFETY findings; the P1 CF-deploy-path finding is round 7b (concurrent).

- R7-P1-2 (recovery-ack migration backfill, DATA-SAFETY). Migration `0004` only CREATES the empty
  `recovery_phrase_acks` table, so any encrypted source created BEFORE `0004` (when the ack gate was
  in-memory only) stays ENABLED with NO durable ack row. After upgrade `list_pending_recovery_acks`
  finds nothing and the scheduler keeps producing encrypted backups for a phrase the user may never
  have saved - unrestorable on a new machine. Fixed with a one-time STARTUP REPAIR (chosen over pure
  SQL because it needs app logic to pick a canonical source + reason about "already-acked"):
  `StateRepo::repair_unacked_encrypted_sources_on_upgrade(now)` (default impl in `state/mod.rs`,
  atomic SQLite override in `state/sqlite.rs`), called from `lib.rs` setup BEFORE `build_and_spawn`
  (so the orchestrator never sees the bad sources as enabled) and BEFORE
  `reconstruct_recovery_acks_from_db` (so the in-memory mirror picks up the freshly-seeded pending
  rows). For every account with a stamped `encryption_master_key_id` that has encrypted source(s) but
  ZERO `recovery_phrase_acks` rows, it DISABLES every encrypted source on the account + seeds a
  PENDING ack row (`revealed=0`) for the earliest-created encrypted source - the same gate the
  first-source path uses; re-reveal + re-ack re-enables. IDEMPOTENCY + the "never disable an
  already-acked source" invariant are guaranteed by a durable `settings` marker
  (`recovery.ack_backfill_v1`): the repair runs EXACTLY ONCE on the first post-upgrade boot. That is
  the only moment a "no ack row" encrypted source must be pre-0004 (the durable flow that DELETES the
  ack row on a legitimate ack did not exist before this version), so disabling it is correct; on every
  later boot the marker short-circuits the repair, so a legitimately-acked source (ack row deleted,
  source enabled) is never re-disabled. The SQLite override does the disable + pending-ack inserts +
  the marker write in ONE transaction (all-or-nothing; a crash re-runs cleanly). Tests in `sqlite.rs`:
  pre-0004 enabled+unacked source -> disabled + pending ack seeded, then re-reveal+ack re-enables;
  idempotent re-run never touches an already-acked source; unencrypted / unstamped / durable-pending
  accounts are skipped.

- R7-P2-1 (reveal-before-click, DATA-SAFETY). `SourceTable.beginRevealAck` called
  `sources.revealRecoveryPhrase` as soon as the post-restart panel OPENED, so the durable
  `revealed=1` backend state could be set even if the user cancelled without seeing the phrase -
  weakening the "revealed == the user actually saw it" invariant. Fixed by moving the
  `revealRecoveryPhrase` IPC OUT of the open handler and INTO a `revealPhraseAction` threaded into
  `RecoveryPhraseReveal` as its `reveal-action`, which fires only on the user's Reveal CLICK (it
  fetches + durably records the reveal AND returns the 24 words, which the action stores into
  `revealPhrase`). `beginRevealAck` is now a sync state-reset + open (no IPC). `RecoveryPhraseReveal`
  gained a `canReveal` gate so the Reveal button is clickable even before a phrase is loaded WHEN a
  `revealAction` is supplied (the action populates the phrase); `everRevealed` still latches only once
  a real phrase is present, so a no-phrase reveal never unlocks ack. Vitest (`settings-components`):
  opening the panel does NOT call `reveal_recovery_phrase`; clicking Reveal inside the panel does;
  opening + cancelling never records a reveal.

  Gates: cargo build/clippy/test --workspace + build -p driven-app + deny + fmt + git diff --check all
  green; `.sqlx` regenerated (0 drift) for the new repair queries; ui pnpm lint + test:unit (149) +
  build (vue-tsc clean). Stub sweep on the touched surface (driven-core/src, src-tauri/src, ui): zero
  non-test `todo!(`/`unimplemented!(`/`unreachable!(`.

## M9 fix round 8 (codex M9-8: 2 P1 + 1 P2 - refinements; closes the M9 codex loop)

Source review: `.claude/codex-reviews/M9-8-20260625-010000.md` (baseline 97f596e, M9 @ b02e519). All 3
legit; both P1s refine prior fixes (R7-P1-2 + R6-P2-1/R1-P2-1). Single sole-actor. After codex recheck
M9-9, M9's per-milestone loop closes; any residual is deferred to the pre-GA whole-repo xhigh capstone
(task #14) + M10.

- R8-P1-1 (upgrade recovery-repair must FAIL CLOSED, DATA-SAFETY). The R7-P1-2 repair
  (`repair_unacked_encrypted_sources_on_upgrade`) was called in `lib.rs` setup but its `Err` only
  LOGGED - setup then proceeded to `assembly::build_and_spawn`, so a pre-0004 enabled encrypted source
  with no durable ack kept SYNCING (the exact unsafe state the repair exists to prevent). Fixed to FAIL
  CLOSED: `assembly::repair_allows_spawn(&result)` (pure; `true` iff `Ok`) gates the boot. On a repair
  error the boot path calls the new `assembly::build_quiesced(state)` instead of `build_and_spawn` - it
  manages the state repo but spawns ZERO orchestrators (no account syncs, encrypted or not), so nothing
  backs up until the repair succeeds. The repair marker stays UNSET on error, so a later boot retries
  and, on success, spawns normally. `reconstruct_recovery_acks_from_db` runs in both paths (the command
  gate stays correct while quiesced), so the user can still reach Settings to reveal/ack. A tray note
  (`tray::notify_repair_failed`, backend i18n `notifications.repair_failed`) tells the user why sync is
  held off. The IPC layer keeps working while quiesced (state is managed). Tests (`assembly.rs`):
  `repair_allows_spawn` is Ok->true / Err->false; `build_quiesced` over a SqliteStateRepo seeded with
  an ENABLED ENCRYPTED account+source spawns ZERO orchestrators (`accounts().is_empty()`) - the
  injected-failure / fail-closed branch starts nothing; the clean spawn path is covered by the live
  lib.rs boot (needs an AppHandle).

- R8-P1-2 (backend macOS gate on ALL auto-update install paths). The macOS in-app-updater guard lived
  ONLY in About.vue, so the dev-channel PERIODIC silent install reached `download_and_install` on macOS
  (and so did `install_update` if invoked). DESIGN: unsigned macOS V1 must use a MANUAL DMG reinstall.
  Fixed with a BACKEND gate `updater::install_disposition(os_is_macos)` (pure; returns
  `ManualOnMacos`/`Install`; production passes `cfg!(target_os = "macos")`, so BOTH arms stay reachable
  + unit-tested on every host and there is NO cfg-gated dead code for 3-OS clippy). The periodic dev
  path: on macOS it records the pending update + emits `updater:available` (so About's macOS guard
  shows the DMG link) + raises `tray::notify_manual_update_available`, NEVER `download_and_install`;
  non-macOS installs silently as before. `install_update`: on macOS short-circuits BEFORE taking the
  pending update (leaves it intact so the About surface still shows the DMG link), raises the manual
  tray note, and returns the new `ErrorCode::UpdateManualRequiredMacos` (`update.manual_required_macos`)
  which the UI renders via `t("errors.update.manual_required_macos.long")`; Windows/Linux unchanged. New
  error code added to `code()` + `from_code()` + classified non-network in `tray::error_code_is_network`;
  i18n keys added (frontend `errors.update.manual_required_macos`, backend
  `notifications.manual_update_available`). Test (`updater.rs`):
  `install_disposition(true) == ManualOnMacos`, `install_disposition(false) == Install`.

- R8-P2-1 (normalize recovery reveal/ack errors). `SourceTable.vue` (onRevealError / confirmRevealAck
  catch) and `AddSourceWizard.vue` (finishReveal ack catch / onPhraseRevealError) stored `String(e)` /
  `String(code)`, so a Tauri STRUCTURED error rendered as `[object Object]` and backend English could
  leak. Fixed to normalize with the existing `toErrorCode(e)` helper into a stable SPEC s24 code stored
  in a `revealErrorCode` ref, rendered via `t(\`errors.${code}.long\`)` (matching the setup/updater
  stores). The reveal/ack codes (`internal.invalid_input`, `crypto.key_missing`) already exist in the
  bundle, so no new reveal/ack i18n keys were needed. Vitest (`recovery-reveal-error-i18n.test.ts`):
  a structured reveal error AND a structured ack error in SourceTable, and a structured reveal error in
  the AddSourceWizard reveal step, each render the localized long message and NOT `[object Object]` /
  the raw backend English.

  Gates: SQLX_OFFLINE cargo build --workspace --all-targets + clippy --workspace --all-targets -D
  warnings + test --workspace (167 app; google_e2e + elevation honest gate-skip) + build -p driven-app
  + deny check + fmt --all --check + git diff --check all green; no sqlx/migration change (no
  sqlx-prepare needed). ui pnpm install + lint + test:unit (152) + build (vue-tsc clean). Stub sweep on
  the touched surface (src-tauri/src, ui): zero non-test `todo!(`/`unimplemented!(`/`unreachable!(`.

## M9 fix round 9 (closeout) (codex M9-9b: fix P1-2 + 3 P2, document P1-1 as accepted residual)

Source review: `.claude/codex-reviews/M9-9b-20260625-014635.md` (baseline 97f596e, M9 @ 4cf769c). This
is the M9 CLOSEOUT round: after it lands + CI green, the M9 per-milestone codex loop CLOSES - no further
M9 codex recheck. Any residual is caught by the pre-GA whole-repo xhigh capstone (task #14) + M10.
Single sole-actor.

- R9-P1-2 (RecoveryPhraseReveal post-restart reveal could latch then get reset, DATA-SAFETY). After
  `await revealAction`, `toggle` checked `hasPhrase` (the parent `phrase` prop) before Vue had delivered
  the prop, and when the prop then arrived the `watch(() => props.phrase)` reset `revealed`/`everRevealed`
  to false. So the post-restart SourceTable reveal/ack path (reachable at v0.1.0 by restarting
  mid-onboarding) could reveal successfully yet keep the ack checkbox LOCKED, leaving a pending encrypted
  source disabled forever. FIXED deterministically: `revealAction` now RETURNS the phrase
  (`() => Promise<string[] | void>`); `toggle` latches `revealed`/`everRevealed` FROM the returned value
  (via a new `latchReveal(phrase)` helper) - it no longer depends on Vue having delivered the prop. The
  prop watcher records the latched words in `latchedPhrase` and SKIPS the re-lock when the incoming prop
  is just the (later-tick) delivery of those SAME words (`samePhrase`), so the latch survives; a
  genuinely DIFFERENT or cleared phrase still re-locks (the R3-P1-1 contract). `ackEnabled` now treats
  "a real phrase is present" as satisfied by the prop OR the latched phrase, so the checkbox is reachable
  across the prop tick. Both parents (`SourceTable.vue`, `AddSourceWizard.vue`) updated to RETURN the
  revealed phrase from their reveal actions. Vitest (`recovery-phrase-reveal.test.ts`): a post-restart
  reveal (empty prop, action returns the words) UNLOCKS the ack and STAYS unlocked across the same-words
  prop delivery (and the user can acknowledge); a DIFFERENT phrase arriving after a latch still re-locks.

- R9-P2-1 (fetch-live-channel.sh fail closed on preservation WRITE errors too, release-pipeline). The
  script used `set -uo pipefail` WITHOUT `-e`, so a failed `mkdir`/`mv` in the channel-preservation
  overlay could still let it exit 0 - the following whole-site Cloudflare Pages deploy would then WIPE
  the other channel's live manifests (channel wipe; broken auto-update for that channel). FIXED:
  `set -euo pipefail` + EXPLICIT checked overlay writes - the `ok` branch now checks `mkdir -p
  "$dest_dir"` and `mv "$tmp" "$dest"`, and on either failure cleans up the temp file, emits a
  `::error::` line, and `exit 1` (fail closed, aborting the deploy; consistent with the existing fetch
  fail-closed policy that tolerates only a genuine 404). Re-validated with a stubbed-curl harness: the
  two existing behaviors still hold (transient non-404 fetch failure -> exit 1; all-local i.e. every
  dest already generated -> exit 0, curl never called), plus all-404 -> exit 0, the new overlay-write
  failure -> exit 1, and the happy path overlays a manifest and exits 0.

- R9-P2-2 (DOCUMENTED DEVIATION - align design docs to the resolved dev-channel contract). The
  `dev-channel.yml` workflow is the RESOLVED-CORRECT contract; DESIGN.md ("per-commit builds from main"),
  SPEC.md s19.4 ("main-push-triggered", "`0.0.0-dev.<short-sha>`"), and ROADMAP.md ("`0.0.0-dev.<sha>`")
  described the EARLIER sketch. The workflow was deliberately changed across M9: (a) GATED on a manual
  `workflow_dispatch` OR a `[dev-build]` head-commit marker (NOT every main push) to bound premium CI
  minutes - most main commits have no business producing a dev installer; (b) the dev version is
  `<next-patch>-dev.<run_number>.<sha>` (R2-P1-1) so it is strictly ABOVE the current stable + MONOTONIC
  across dev builds (the old `0.0.0-dev.<sha>` was LOWER than stable `0.1.0`, so a stable user opting into
  dev was never offered an update). The workflow was NOT reverted; instead the docs were updated to
  match: DESIGN.md s3.6 update-channels bullet, SPEC.md s19.4 + the dev-channel.yml tree comment, and
  ROADMAP.md M9 acceptance. This is a deliberate documented deviation from the original SPEC sketch.

- R9-P2-3 (About.vue macOS manual-download link must follow the channel). The macOS DMG download link
  was hardcoded to the stable `/releases/latest`, so a dev-channel macOS user shown a dev update banner
  was sent to STABLE. FIXED: a `macDownloadUrl` computed derives the URL from `updater.available.channel`
  - `dev` -> `/releases/tag/dev` (the rolling dev release tag dev-channel.yml publishes via
  softprops/action-gh-release), otherwise `/releases/latest` (defaults to stable when no update is in
  hand). Vitest (`about-mac-gating.test.ts`): a dev-channel available update links to `/releases/tag/dev`
  and a stable one to `/releases/latest`.

- R9-P1-1 (ACCEPTED M9 RESIDUAL - NOT fixed this round). `crates/driven-core/src/state/sqlite.rs`
  (~:1075 disable-all, :1105 single ack row, :1001 re-enable-one): the pre-0004 upgrade repair disables
  ALL encrypted sources for an account but creates an ack row for / re-enables only the EARLIEST source.
  A pre-0004 account with MULTIPLE encrypted sources would leave previously-enabled siblings disabled
  forever after the one visible ack; if the earliest encrypted source was intentionally disabled, ack
  flips it on. This is NOT reachable at the v0.1.0 first GA: there is NO pre-0004 install base - every
  fresh install creates the 0004 table from the start and routes every encrypted source through the
  proper per-source gate, so the disable-all/ack-one repair path is never entered. ACCEPTED as an M9
  residual; revisit in the pre-GA whole-repo xhigh capstone (task #14) / post-GA. The correct fix is to
  record each source's ORIGINAL enabled-state and restore ALL affected sources atomically on the
  account-level ack (or create per-source ack rows for every enabled encrypted source that was disabled).

  Gates: SQLX_OFFLINE cargo build --workspace --all-targets + clippy --workspace --all-targets -D
  warnings + test --workspace (google_e2e + elevation honest gate-skip) + build -p driven-app + deny
  check + fmt --all --check + git diff --check all green; no sqlx/migration change. ui pnpm install +
  lint + test:unit + build (vue-tsc clean). fetch-live-channel.sh re-validated via a stubbed-curl
  harness (no committed shell test; no .yml changed, so no actionlint run). Stub sweep on the touched
  surface (ui, scripts, src-tauri/src): zero non-test `todo!(`/`unimplemented!(`/`unreachable!(`.

## M9b - telemetry (anonymous usage stats: client + Cloudflare Worker)

  SPEC s16: an opt-out-able anonymous usage ping (client) + the Cloudflare Worker that ingests it into
  Analytics Engine. DEFAULT ON, one click off, honored immediately, privacy-preserving.

  CLIENT (`src-tauri/src/telemetry.rs`):
  - `install_id`: a UUID v4 anonymous id. The migration 0002 seed already writes a random
    `telemetry.install_id`; `ensure_install_id` additionally MINTS a UUID v4 + persists it (preserving
    siblings) if the stored field is empty (a pre-seed DB / cleared field), so it is always non-empty and
    STABLE across restarts (test: `install_id_persists_across_reload`).
  - `telemetry.enabled` pref, DEFAULT ON: a missing/malformed flag reads as enabled. When OFF the send
    path makes NO network call at all and the toggle is honored on the very next tick (the loop re-reads
    the pref each iteration) - test `disabled_telemetry_sends_no_ping` asserts zero sink calls.
  - Cadence: startup + every 24h via a tokio `interval` (NOT a sleep/poll loop), joined into the M5
    shutdown drain (AppState `TelemetryRuntime` { task, shutdown } + `set_telemetry_task` /
    `shutdown_telemetry_task`; lib.rs spawns it in setup after `manage(app_state)` and drains it on quit
    via the bounded abort-capable `drain_restore_handle`, mirroring the M9a updater task - no orphan).
  - Payload (SPEC s16): `{ install_id, ts, version, os, os_version?, arch, channel, events_24h: {
    files_uploaded, bytes_uploaded, errors_by_class: {<code>: n}, deep_verify_runs, update_applied },
    latency_p50_p95_ms: { scan: [], upload_per_mb: [] } }`. version = `app.package_info().version`; os/arch
    = `std::env::consts::{OS,ARCH}`; channel from the updater `channel` setting.
  - events_24h aggregation (`StateRepo::telemetry_events_24h(since_ms)`, SQLite override, read-only,
    bounded): files_uploaded/bytes_uploaded = count/SUM(bytes) of `activity_log` `upload_done` rows in
    `[now-24h, now]`; errors_by_class = COUNT grouped by `event_type` of `error`-level rows (event_type IS
    the SPEC s24 error code for a failed op - a fixed dotted-code enum, never user data); deep_verify_runs
    = COUNT of `backup_sources` whose `last_deep_verify_at` falls in-window; update_applied = COUNT of any
    `update_applied` activity rows (0 in V1 - no such row is written yet; the aggregate picks it up
    automatically if/when one is). Test `telemetry_events_24h_aggregates_uploads_errors_and_deep_verify`.
  - Best-effort send: the POST is behind a `TelemetrySink` trait (the offline test seam); production
    `HttpTelemetrySink` is a single bounded-timeout (15s) reqwest POST. A failure (network/timeout/non-2xx)
    is logged at info + SWALLOWED - it NEVER affects backups, never surfaces, never retries in a storm
    (test `send_error_is_swallowed_and_non_fatal`). Tests NEVER hit the live endpoint.
  - IPC: `get_telemetry_enabled` / `set_telemetry_enabled` / `get_telemetry_install_id`, mirrored into
    `ui/src/ipc/commands.ts`. UI: a "Send anonymous usage stats" toggle (default ON) + a short i18n privacy
    note in Settings > Rules, round-tripping `{ telemetry: { enabled } }` through the settings store; vitest
    asserts default-ON + toggle-patches.

  PRIVACY REVIEW (load-bearing): the payload carries ONLY counts, sizes, error CODES, and (empty in V1)
  latency arrays, plus the anonymous random `install_id` + a platform descriptor + the channel. It does
  NOT carry, and the aggregation SQL does NOT select, any file name, path, message, account email, or
  content. The `activity_log.message` column (which holds a relative path for per-op rows) is never read
  by `telemetry_events_24h`. A unit test (`payload_has_the_spec_s16_shape_and_carries_no_paths`) asserts
  the serialized JSON's top-level keys are exactly the SPEC s16 set and contains no `path`/`message`/file
  name. `os_version` is honestly `None` in V1 (no OS-version crate dependency - dependency-minimalism +
  privacy; it is a nullable field skipped when None). Latency percentiles are emitted as EMPTY arrays
  rather than fabricated, because V1 records no per-op durations in durable state; real latency capture is
  a later instrumentation-bearing change (the keys are present so the wire shape is stable).

  WORKER (`telemetry-worker/`, OUTSIDE the cargo workspace + the ui build - its own TS/wrangler toolchain;
  added to the root Cargo.toml `exclude`):
  - `POST /telemetry/v1/ping` -> validate the JSON shape (`validatePing`: strict on install_id/version/os/
    arch/channel + the non-negative numeric aggregates; tolerant of absent os_version + empty latency
    arrays) -> `writeDataPoint` to the `TELEMETRY` Analytics Engine dataset binding (indexes=[install_id];
    blobs=[os, arch, channel, version, errors JSON]; doubles=[files, bytes, deep_verify, update_applied,
    total_errors, ts]) -> 204. Malformed body/JSON/oversized -> 400; wrong method -> 405; any other path ->
    404. Body size capped at 16 KB (declared Content-Length + actual length). Never logs the raw body (an AE
    write error logs a GENERIC message + still returns 204 so the best-effort client does not retry-storm).
  - Routing (SPEC s16): `routes: [{ pattern: "driven.maxhogan.dev/telemetry/*", zone_name: "maxhogan.dev" }]`
    + `workers_dev: false` on the Driven account (id 9c20c14daa20466a2d761a47162f719a). A path-scoped
    Worker route takes precedence over the CF Pages site on the same hostname for the `/telemetry/*` prefix,
    so Pages keeps serving the root + /updates while this Worker owns /telemetry/* (confirmed via Context7
    CF docs: custom-domain/zone-scoped routes + path patterns).
  - Tested: a unit test of `handle(request, env)` against a MOCKED AE binding (a `writeDataPoint` spy) -
    valid POST -> 1 write + 204; malformed/negative-aggregate/oversized -> 400; wrong method -> 405; wrong
    path -> 404; AE-throw -> still 204 + generic log. tsc (`--noEmit`) + eslint clean; `wrangler types`
    parses the config + emits the `TELEMETRY: AnalyticsEngineDataset` Env. 11 worker tests pass.

  DEFERRED (M10/ops): the actual `wrangler deploy` + the live e2e telemetry validation (a real ping landing
  in Analytics Engine) need CF creds + a real deploy. This round IMPLEMENTS + statically validates only
  (tsc / wrangler types / a mocked-binding handler unit test). The dataset `driven_telemetry` is created on
  first `writeDataPoint` at deploy time; the CF project `driven-telemetry` is already provisioned.

  Gates: SQLX_OFFLINE cargo build --workspace --all-targets + clippy --workspace --all-targets -D warnings
  + test --workspace (google_e2e + elevation honest gate-skip) + build -p driven-app + deny check + fmt
  --all --check + git diff --check all green; sqlx-prepare run (4 new query files, 0 drift). ui pnpm
  install + lint + test:unit + build (vue-tsc clean). worker: pnpm install + tsc + eslint + vitest green.
  Stub sweep on the touched surface (src-tauri/src, ui, telemetry-worker): zero non-test
  `todo!(`/`unimplemented!(`/`unreachable!(`.

## M9b fix round 1

  Codex M9b xhigh review (`.claude/codex-reviews/M9b-20260625-025031.md`, baseline 8bb3fe9, M9b @ 6da67ab):
  4 P1 + 3 P2 telemetry findings, all legit, all fixed as ONE sole-actor pass (client + worker are a
  COUPLED payload contract, kept byte-consistent). Spec: `.claude/m9b-fix-spec.md`.

  - P1-1 (PUBLIC worker hardening, `telemetry-worker/src/index.ts`): the endpoint is public, so
    `validatePing` now REJECTS anything that could persist PII / high-cardinality junk into Analytics
    Engine. `install_id` must match a UUID v4 regex (`UUID_V4`); `channel`/`os`/`arch` must be in closed
    whitelists (`{stable,dev}` / `{windows,macos,linux}` / `{x86_64,aarch64}`); `version` + `os_version`
    are length-bounded (64); `errors_by_class` keys must be in the SPEC s24 dotted-code set (`ERROR_CODES`,
    mirrors `crates/driven-core/src/types.rs` `ErrorCode::code()` exactly - 41 codes), with the key COUNT
    capped (`MAX_ERROR_CLASSES = 64`) and each value a bounded non-negative integer
    (`MAX_ERROR_COUNT = 1e9`). Any violation -> 400, no AE write. Negative tests added: path-shaped +
    email-shaped + non-v4 `install_id`, junk `channel`/`os`/`arch`, a path-shaped + email-shaped
    `errors_by_class` key, a high-cardinality flood, an out-of-range value, over-long `version`/`os_version`.
  - P1-2 (honor disable immediately, `telemetry.rs` `maybe_send_once` + `app_state.rs`): the pref is now
    RE-READ immediately before `sink.send` (abort with no network call if disabled mid-build), AND an
    `AppState` `TelemetryRuntime.cancel` `AtomicBool` is checked at entry + right before send;
    `set_telemetry_enabled(false)` flips it to `true` BEFORE the write (re-armed to `false` on re-enable),
    so a disable during the id-ensure / aggregate / build window aborts the in-flight send. Tests:
    `disabling_between_initial_check_and_send_aborts_the_send` (cancel flag) +
    `disabling_pref_mid_build_is_honored_by_the_immediate_reread`.
  - P1-3 (install_id is a UUID v4, migration 0002 + `telemetry.rs`): the seed now builds a canonical UUID
    v4 in SQL from `hex(randomblob(16))` (version nibble forced to `4`, variant nibble from `89ab`), and
    `ensure_install_id` REPLACES any empty / legacy non-UUID-v4 value (e.g. the old bare
    `hex(randomblob(16))`) with a fresh UUID v4 ONCE, leaving a valid v4 untouched (stable). New
    `is_uuid_v4` byte-checker (no regex crate) shared in spirit with the worker. Tests:
    `seeded_install_id_is_a_uuid_v4`, `legacy_non_uuid_install_id_is_replaced_with_uuid_v4_then_stable`,
    `is_uuid_v4_accepts_canonical_and_rejects_junk`, and `settings_round_trip` now asserts the 36-char v4
    shape.
  - P1-4 (update_applied is a BOOLEAN per SPEC s16, client + worker): `Events24h.update_applied` is now
    `bool` (`aggregate.update_applied > 0`); the worker validates `typeof === "boolean"` and maps it to the
    `0/1` AE double. SPEC unchanged. Tests assert the JSON serializes as a literal boolean on both sides.
  - P2-1 (coarse os_version, `telemetry.rs` + `Cargo.toml` + `deny.toml`): added the `os_info` crate (MIT,
    `default-features = false`; covered by the existing MIT deny allowance - no new allowance needed) and a
    `coarse_os_version()` helper that returns a bounded (<=64), non-"Unknown" version string or `None`.
    `os_version` is now ALWAYS serialized (the `skip_serializing_if` was removed) - present when known,
    `null` otherwise - so the wire contract is stable. The worker persists it as a 5th blob (`""` when
    absent). Tests: `os_version_always_serialized_even_when_none`,
    `coarse_os_version_is_a_short_non_path_string_or_none`, worker `persists a coarse os_version blob`.
  - P2-2 + P2-3 (bound-at-now + DELTAS, `state/mod.rs` + `sqlite.rs` + `telemetry.rs`): `telemetry_events_24h`
    is renamed to `telemetry_events_since(since_ms, now_ms)` and every sub-query now bounds the UPPER end at
    `now_ms` (`ts >= ?since AND ts <= ?now`, incl. the `last_deep_verify_at` query), excluding clock-skewed
    future rows (P2-2). The client records `telemetry.last_sent_at` on each SUCCESSFUL send and computes the
    next window via `delta_since_ms(now, last_sent) = max(now-24h, last_sent+1)`, so each ping reports only
    events in `(last_sent, now]` (capped at 24h for the first send / after a long gap), and a same-instant
    restart double-counts NOTHING (P2-3 subsumes P2-2's upper bound). Tests:
    `delta_since_caps_first_window_and_excludes_last_sent`,
    `delta_aggregation_does_not_double_count_across_restarts`, and the renamed
    `telemetry_events_since_aggregates_uploads_errors_and_deep_verify` (now asserts both a future activity
    row AND a future deep-verify source are excluded, plus a degenerate `since > now` window yields zero).

  Gates (all green): SQLX_OFFLINE cargo build --workspace --all-targets + clippy --workspace --all-targets
  -D warnings + test --workspace (208+ pass) + build -p driven-app + deny check + fmt --all --check + git
  diff --check; sqlx-prepare re-run (4 telemetry query files swapped for the bounded versions, 0 drift). ui
  pnpm install + lint + test:unit (157) + build (vue-tsc clean). worker: pnpm install + tsc + eslint +
  vitest (27 tests, incl. the new PII-rejection negatives). Stub sweep on the touched surface: zero
  non-test `todo!(`/`unimplemented!(`/`unreachable!(`.

## M9b fix round 2

  Codex M9b recheck-1 (`.claude/codex-reviews/M9b-recheck-20260625-032935.md`, baseline 8bb3fe9, M9b @
  4ba7b22): 2 P1 + 3 P2, all legit refinements of round 1. ONE sole-actor pass (client + worker stay a
  coupled, byte-consistent payload contract). Spec: `.claude/m9b-fix-spec-r2.md`. Telemetry closes after
  this + recheck-2.

  - R2-P1-1 (opt-out via EVERY renderer path, `telemetry.rs` + `commands/settings.rs` + `stores/settings.ts`
    + `Settings.vue` + `About.vue`): round 1 only flipped the in-flight cancel flag in the dedicated
    `set_telemetry_enabled` command, but the Settings/About toggles called the generic `update_settings`,
    so a disable click while a ping was building could still send. FIX: a SHARED
    `telemetry::apply_enabled_change(state, cancel, enabled)` helper now owns the one cancel-preserving path
    - it flips the `AppState` `TelemetryRuntime.cancel` `AtomicBool` (set BEFORE the write when disabling,
    cleared on enable) then does the preserving `write_enabled`. BOTH `set_telemetry_enabled` AND the
    `update_settings` telemetry branch route through it, so any backend path honors opt-out immediately. The
    UI toggles (Settings + About) now call the dedicated `set_telemetry_enabled` IPC via a new
    `settingsStore.setTelemetryEnabled(enabled)` action (which updates the snapshot in place), not the
    generic patch. Tests: `apply_enabled_change_off_trips_cancel_flag_and_persists` (Rust),
    `settings-stores` `setTelemetryEnabled calls set_telemetry_enabled ...`, and the `settings-components`
    toggle test now asserts `set_telemetry_enabled` is called and `update_settings` is NOT.
  - R2-P1-2 (content-validate version + os_version, `telemetry-worker/src/index.ts`): round 1 only
    length-checked these two blobs, so short PII (`alice@example.com`, `/home/alice`) fit. FIX: strict
    content allowlists in ADDITION to the length cap. `VERSION_RE` is a pragmatic semver-ish allowlist
    (`MAJOR.MINOR.PATCH` + optional dot-separated `[0-9A-Za-z-]` prerelease + optional `+build`) that
    ACCEPTS the client's REAL `package_info().version` outputs - plain `0.1.0` on stable and
    `0.1.1-dev.<run>.<sha>` on the CI dev channel (verified against `telemetry.rs`). `OS_VERSION_RE` allows
    only coarse platform chars (ASCII alnum, dot, hyphen, underscore, single inter-token spaces; leading +
    trailing char alphanumeric; whitespace-RUNS rejected) - accepts the real `os_info` coarse outputs
    (`11.26200`, `14.5`, `10.0.19045`, `22.04 LTS`, `rolling`, codenames). Both REJECT `/`, `\`, `@`, control
    chars -> 400. Negatives added: email/path/backslash-shaped version, email/path-shaped + whitespace-run
    os_version; positives assert the real stable + dev version shapes and the real os_info shapes are
    accepted.
  - R2-P2-1 (preserve `last_sent_at` on a settings toggle, `commands/settings.rs`): the narrow
    `storage::Telemetry` struct omitted `last_sent_at`, so an `update_settings` telemetry patch
    deserialized the group and wrote back only `enabled/install_id/endpoint`, DROPPING the delta checkpoint
    (-> duplicate windows after a toggle). FIX: the `update_settings` telemetry branch no longer round-trips
    through `storage::Telemetry`; it routes through `apply_enabled_change`, whose preserving
    read-modify-write mutates ONLY `enabled` and keeps `install_id` / `endpoint` / `last_sent_at` /
    `last_recorded_version` intact. (`storage::Telemetry` is still used by the read-only `get_settings`
    display path, which never persists, so no clobber.) Test:
    `update_settings_telemetry_patch_preserves_last_sent_at`.
  - R2-P2-2 (integer + range validation on numeric fields, `telemetry-worker/src/index.ts`): round 1
    accepted any finite non-negative number for `files_uploaded` / `bytes_uploaded` / `deep_verify_runs` /
    `ts`, so fractions + huge finite doubles slipped in. FIX: `isBoundedCount` now requires
    `Number.isSafeInteger` (rejects fractions AND non-safe-integer doubles) under per-field caps
    (`MAX_FILES_UPLOADED = 1e9`, `MAX_BYTES_UPLOADED = 1 PiB`, `MAX_DEEP_VERIFY_RUNS = 1e6`); `ts` is
    validated by `isIntegerInRange` to a plausible epoch-ms window (`TS_MIN_MS` 2020-01-01 .. `TS_MAX_MS`
    2100-01-01), catching a seconds-vs-ms mistake too. Any violation -> 400. Negatives added: fractional
    files/bytes/deep_verify/ts, huge non-safe-integer bytes + ts, over-cap bytes, seconds-granularity +
    far-future ts.
  - R2-P2-3 (actually record `update_applied`, `telemetry.rs` + `lib.rs`): nothing wrote an `activity_log`
    row with `event_type='update_applied'`, so the aggregate was always 0/false. FIX: a new
    `telemetry::record_update_applied_if_changed(state, running_version, now_ms)` persists the last-observed
    app version in the `telemetry` group (`last_recorded_version`, preserved by the read-modify-write
    helpers). On boot (lib.rs setup, after `manage(app_state)` + before the first ping is spawned) it
    compares the running `package_info().version` to the stored one: a FIRST run with no recorded version
    just seeds it (a fresh install is NOT an update, no event); a CHANGED version writes exactly ONE
    `update_applied` Info activity row and advances the stored version; an UNCHANGED version writes nothing.
    Cheap, idempotent, fully non-fatal (every error logged + swallowed, never blocks boot). The existing
    `telemetry_events_since` aggregate counts these rows, so the ping field is now driven by a real path.
    Test: `update_applied_recorded_exactly_once_on_version_change` (fresh -> 0, same -> 0, change -> 1,
    re-boot -> still 1; aggregate == 1).

  Gates (all green): SQLX_OFFLINE cargo build --workspace --all-targets + clippy --workspace --all-targets
  -D warnings + test --workspace (telemetry 19 incl. the 3 new; full suite pass, only the honest
  elevation/google_e2e gate-skips ignored) + build -p driven-app + deny check + fmt --all --check + git
  diff --check. No `sqlx::query!` / migration change this round (the update_applied path reuses
  `write_activity` + the existing `telemetry_events_since` query), so no sqlx-prepare needed - 0 drift. ui
  pnpm install + lint + test:unit (159) + build (vue-tsc clean). worker: pnpm install + tsc + eslint +
  vitest (44 tests, incl. the new version/os_version PII negatives + real-shape acceptance + numeric
  range negatives). Stub sweep on the touched surface (telemetry.rs, settings.rs, lib.rs, index.ts,
  settings.ts, Settings.vue, About.vue): zero non-test `todo!(`/`unimplemented!(`/`unreachable!(`.
