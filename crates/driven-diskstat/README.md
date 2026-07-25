# `driven-diskstat`

Answers one question for the adaptive upload-parallelism controller: *is the local
disk saturated right now?* If it is, adding in-flight uploads cannot raise
throughput, so the controller must not grow the pool (`design/DESIGN.md` s11.4.7,
s18.2).

- `lib.rs` - the `DiskBusyProbe` trait, the `DiskBusy` reading, and the pure
  classifier (`busy_fraction_from_delta`, `SATURATION_THRESHOLD` = 80 %) that is
  unit-tested on every target
- `linux.rs` (`/proc/diskstats`), `macos.rs` (IOKit), `windows.rs` (PDH
  `% Disk Time`) - exactly one `RealDiskBusyProbe` compiled per target

Kept a standalone crate rather than folded into `driven-core` so the PDH / IOKit FFI
never reaches a foreign host's `cargo build --workspace`. The fail-open rule is
load-bearing: an unreadable probe returns `Unknown`, which reads as "not saturated",
because a broken reader must never pin uploads small. Tests use `FakeDiskBusyProbe`
from `driven-test-fixtures`.

```sh
cargo test -p driven-diskstat
```
