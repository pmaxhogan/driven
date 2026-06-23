# CODEX notes - M3.7 codex-review fixes (round 1)

Residual / design decisions from the 9 P1+P2 fixes (see `.claude/m3.7-codex-fix-spec.md`).

## P1-E disk-full-target - tracked known-gap (NOT fake-green)

The core out-of-space mapping IS implemented and unit-tested: `executor::local_io_error_code`
/ `is_disk_full` map ENOSPC (errno 28, Unix), `ERROR_DISK_FULL` (112) / `ERROR_HANDLE_DISK_FULL`
(39) (Windows), and `std::io::ErrorKind::StorageFull` to `ErrorCode::LocalDiskFull`
(test `enospc_classifies_as_local_disk_full`). The earlier "core maps out-of-space to
local.io_error" gap is CLOSED.

The `disk-full-target` scenario remains capability-gated (`Capability::Admin`) and bails honestly
in `setup`, because a V1 source is READ-ONLY: the executor reads source files and writes to Drive
via `RemoteStore::create`/`update`; it never writes back into the source volume (verified - no
local `File::create` on the source path in the executor). So a read-only source on a 0-free
constrained volume produces no local write, hence no ENOSPC, hence the (now-present, now-tested)
mapping is not reachable end-to-end through V1's source-read path. Driving this row needs a future
write-into-source path (local staging / VSS-temp spool on the source volume) that V1 does not have.
The scenario does not fabricate a pass or assert a code the read path cannot emit.

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
