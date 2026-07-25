# `driven-chaos`

The stress / chaos harness. It boots the headless core directly (never `src-tauri`)
and hammers it with hostile inputs and injected faults. Design and the scenario
catalogue live in `design/STRESS_HARNESS.md` - this README is orientation only.

- `handle.rs` - `DrivenHandle`, one booted headless instance under test
- `scenario.rs` + `registry.rs` + `dispatch.rs` - the `Scenario` trait, the registry
  the runner iterates, and the dispatch
- `capabilities.rs` - the host capability probe; rows the host cannot satisfy
  (admin, VSS, real Drive, wrong OS) skip cleanly rather than fail
- `scenarios/` - the catalogue, one module per category: `filenames`, `file_size`,
  `permissions`, `ntfs`, `concurrency`, `storage`, `drive_side`, `mutation`
- `mutator.rs` + `runner.rs` + `reporting.rs` - the seeded fuzz mutation loop and the
  per-scenario verdict / run report

```sh
just chaos              # full hermetic sweep
just chaos-fake-drive   # the fault-injection subset CI gates on
just chaos-fuzz         # seeded continuous-mutation soak
```
