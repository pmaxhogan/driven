# `crates/` - the Rust workspace members

Everything Driven does that is not the Tauri shell. The split follows the
thick-core / thin-shell rule in `design/DESIGN.md` s4.2: `driven-core` owns the
sync engine and stays I/O-free, and every side effect sits in its own crate
behind a trait, so `cargo test --workspace` runs with no real Drive, clock, or GUI.

- `driven-core` - scanner, planner, orchestrator, pacer, executor, SQLite state
- `driven-drive` - the `RemoteStore` trait, the Google Drive backend, the in-memory fake
- `driven-crypto` - content + filename encryption, keystore, BIP39 recovery phrase
- `driven-net` / `driven-tls` - network probes; shared custom-CA + proxy support
- `driven-power` / `driven-diskstat` - per-OS power, metered, sleep-wake, disk-busy signals
- `driven-vss` / `driven-vss-helper` - Windows shadow-copy reads and the elevated broker
- `driven-cli` - headless debugging CLI; `driven-chaos` - the stress harness
- `driven-test-fixtures` - shared dev-dependency fakes (`tree!`, `FakeClock`, ...)

Dependencies point one way: `driven-tls` / `driven-crypto` are leaves, `driven-core`
sits above drive / crypto / power / diskstat / vss, and `src-tauri` on top of all of
them. Nothing here depends on `src-tauri`. Each crate's `src/lib.rs` module doc is
the real reference - `cargo doc --open`.
