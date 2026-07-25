# `driven-test-fixtures`

Shared test helpers, pulled in as a `[dev-dependencies]` entry by sibling crates and
the workspace integration tests. It is what makes the injected-trait design pay off:
every seam in the workspace has a deterministic fake here.

- `tree.rs` - the `tree!` macro for declaring a temp directory tree inline
- `clock.rs` - `FakeClock` (`driven_core::time::Clock`) with `advance()` / `now_set()`
- `power.rs` - `FakePowerSource` with a `set()` driver for state transitions
- `diskstat.rs` - `FakeDiskBusyProbe` for the adaptive-parallelism tests
- `network.rs` - `FakeNetwork`, simulating offline, captive portal, lossy links, and
  per-service outages
- `assert.rs` - `assert_remote_eq!`, a snapshot diff over remote-store listings

Never a normal dependency of anything, and `publish = false`. Add a fake here rather
than re-rolling one per crate.

```sh
cargo test -p driven-test-fixtures
```
