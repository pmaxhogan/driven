# `driven-power`

Battery / AC / metered-network / sleep-wake signals, normalized across OSes. The
orchestrator's pause-and-resume rules (`design/SPEC.md` s10, `design/DESIGN.md` s5.7
and s5.10.1) are driven entirely by what this crate reports.

- `lib.rs` - the `PowerSource` trait, the `PowerState` snapshot, and the
  `SleepWakeEvent` edge; both subscriptions are `tokio::sync::broadcast` receivers
  because several consumers (state machine, tray, activity log) fan out from them
- `windows.rs` / `macos.rs` / `linux.rs` - exactly one is compiled per target, each
  exporting `RealPowerSource` (re-exported cfg-free so callers need no `cfg`)
- `network.rs` - shared metered / reachability detection used by every backend

Steady state is a 30 s poll; sleep and wake are OS notification edges, not polls.
Tests use `FakePowerSource` from `driven-test-fixtures`. `driven-diskstat` is
deliberately shaped the same way.

```sh
cargo test -p driven-power
```
