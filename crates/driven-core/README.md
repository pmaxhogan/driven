# `driven-core`

The I/O-free heart of Driven: the sync state machine and everything that decides
*what* to back up and *when*. All real I/O (filesystem, network, clock, keychain,
power) arrives through injected traits, so the whole crate is exercisable from
`cargo test` with no Tauri shell, no real Drive, and no real wall clock.

- `scanner.rs` + `exclude.rs` - walk the source tree, apply the exclusion rules
- `planner.rs` - diff scan results against state into a plan of operations
- `orchestrator.rs` + `pacer.rs` + `executor.rs` - run that plan, paced by the rules
- `adaptive.rs` - upload-parallelism controller (throughput + disk-busy driven)
- `state/sqlite.rs` + `migrations/` - the SQLite state layer and its schema
- `types.rs` - the shared `OrchestratorState` machine, event, and error types

The traits declared here (`Clock`, `StateRepo`, `NetworkProbe`, `SourceWatcher`,
`CryptoProvider`) are implemented by the sibling crates; `src-tauri` assembles them.
See `design/DESIGN.md` s5 and s11 for the engine design.

```sh
cargo test -p driven-core
just sqlx-prepare   # after changing any sqlx::query! macro
```
