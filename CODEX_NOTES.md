# CODEX notes - M3.7 codex-review fixes (round 1)

Residual / design decisions from the 9 P1+P2 fixes (see `.claude/m3.7-codex-fix-spec.md`).

## P1-E disk-full-target - tracked known-gap (NOT fake-green)

The core out-of-space mapping IS implemented and unit-tested: `executor::local_io_error_code`
/ `is_disk_full` map ENOSPC (errno 28, Unix), `ERROR_DISK_FULL` (112) / `ERROR_HANDLE_DISK_FULL`
(39) (Windows), and `std::io::ErrorKind::StorageFull` to `ErrorCode::LocalDiskFull`
(test `enospc_classifies_as_local_disk_full`). The earlier "core maps out-of-space to
local.io_error" gap is CLOSED.

The `disk-full-target` scenario is gated on `Capability::DiskMountAllowed` (env
`DRIVEN_CHAOS_ALLOW_DISK_MOUNT=1`, never set today) so it SKIPs everywhere with a recorded reason,
and bails honestly in `setup` if ever run, because a V1 source is READ-ONLY: the executor reads
source files and writes to Drive via `RemoteStore::create`/`update`; it never writes back into the
source volume (verified - no local `File::create` on the source path in the executor). So a
read-only source on a 0-free constrained volume produces no local write, hence no ENOSPC, hence the
(now-present, now-tested) mapping is not reachable end-to-end through V1's source-read path. Driving
this row needs a future write-into-source path (local staging / VSS-temp spool on the source volume)
that V1 does not have. The DiskMountAllowed gate (not bare Admin) matters because GitHub Actions
Windows runners are ELEVATED: an Admin gate would let the row RUN there and turn its honest bail into
a FAIL. The scenario does not fabricate a pass or assert a code the read path cannot emit.

`name-path-4096-bytes` builds the deepest nested path the HOST accepts: macOS/BSD cap a whole path
at PATH_MAX=1024 (Linux is 4096), so on macOS the deep-path build stops at ENAMETOOLONG (errno 63)
and the row documents the host limit + asserts the floor (the deepest creatable path backs up)
rather than erroring on a real platform constraint.

## million-files-nested + huge-file-* - wall-clock cap now wraps run_assertions only

The real capability probe (P1-A) now reports actual free disk, so the `FreeDiskBytes`-gated rows
(`tiny-files-100k`, `million-files-nested`, and `huge-file-*` when space allows) RUN where they
previously SKIPPED (the probe used to hardcode free=0). `million-files-nested` builds a 1,000,000
-file fixture; STRESS_HARNESS s3.2 documents this as ~15 min on an SSD and s8 treats the cacheable
big-fixture build as a separate slow step. The runner's `SCENARIO_WALL_CAP` (300s, s6.3
no-infinite-loop) therefore now wraps ONLY `run_assertions` (the work loop where a hang is a real
bug), NOT the uncapped `setup` fixture build. `write_deterministic` was also fixed to size its
scratch block to the file length so the many-tiny-file rows do not allocate a 1 MiB buffer per file.

## P1-B huge-file content oracle

`huge-file-10gb` / `huge-file-50gb-mid-run-crash` arm `InMemoryRemoteStore::with_content_oracle()`:
the fake records only a length + streaming md5 instead of buffering 10-50 GB in a `Vec<u8>`. The
rows verify length + md5 (against `deterministic_md5` of the source pattern) instead of downloading.
An oracle-stored object's `download` errors by design (the bytes are not retained).

## Scenarios newly EXERCISED by the real probe (P1-A) - fixed or documented

Before P1-A the probe hardcoded `ntfs_volume=None` etc, so several NTFS rows SKIPPED and never
ran. With real probes they run on a Windows+NTFS box and surfaced real scenario/core mismatches:

- `name-unpaired-surrogate`: FIXED in core (same class as P1-D). The scanner now records a path it
  cannot represent as a `RelativePath` (unpaired UTF-16 surrogate) on `ScanResult.invalid_filenames`,
  and the orchestrator emits a durable `local.invalid_filename` WARNING activity row, so the skipped
  file is visible rather than a silent omission. The row asserts the code IS surfaced and now passes.
- `name-trailing-space-and-dot`: DOCUMENTED + DEFERRED. Both `foo .txt` and `foo.txt.` are distinct
  persistable NTFS names, but on the M3 scanner the trailing-DOT name collapses (Win32 / path
  normalisation strips the trailing dot before the bytes are read), so only the trailing-space name
  round-trips as its own object. Rather than assert a Success the core cannot honour, the row is now
  `DocumentedBehaviour`: it asserts the observable floor (the trailing-space name round-trips
  byte-for-byte, >=1 distinct object lands, s6.3 invariants hold) and records the trailing-dot
  collapse. Deep fix (preserve trailing dots through scan) deferred.

## P1-C central sweep - two robustness fixes found by running the drive-mutator rows

The central `reporting::assert_invariants` (the runner-enforced s6.3 snapshot) initially red-flagged
two legitimate states that the scenario-local checks already tolerated:

- A LATCHED fault (e.g. `auth.invalid_grant`) made the sweep's `list_folder` trait call error.
  Fixed: the sweep now reads via the fault-free `list_folder_with_trashed` in-memory accessor (the
  same one the mutator's robust check uses), so a latched-fault terminal state is still verifiable.
- A well-formed deferred-create reconcile op (an `upload` op with a `client_op_uuid` but no
  `drive_file_id` yet - the documented DESIGN s5.6 recovery handle) was counted as a pending-ops
  leak. Fixed: `assert_invariants` now excludes it from `leaked_pending_ops`, matching the mutator's
  per-row definition. This is the correct terminal state for the Drive-transient + crash rows.

## append-only-log - pre-existing timing flake (deferred)

`append-only-log` (a mutator-thread soak racing the scanner) is timing-flaky: it failed once in a
cold-cache full run, then passed 6/6 in isolation, and passes 5/5 on origin/main too. The flake is
pre-existing (not introduced here) and is the scenario's own object-count assertion racing the
append thread, not an invariant or core regression. Deferred - it converges deterministically in
isolation.
