# `src-tauri/`

The Tauri shell - the thin half of the thick-core / thin-shell split. It owns the
desktop process (boot, tray, window, plugins, updater, logging) and exposes the
engine in `crates/` to the `ui/` webview over IPC. No sync logic lives here.

- `src/lib.rs` - the boot path: plugin wiring in the `design/SPEC.md` s14 order,
  migrations, orchestrator assembly, tray, deep links, clean shutdown
- `src/assembly.rs` + `src/app_state.rs` - wires the concrete backends (drive,
  crypto, power, diskstat, net, vss) into the core traits and holds them
- `src/commands/` - every `#[tauri::command]`, one module per surface (`sync`,
  `sources`, `settings`, `accounts`, `activity`, `restore`, `dialogs`,
  `exclusion_stream`, `frontend_log`) plus the `dtos.rs` the UI's `ipc/types.ts` mirrors
- `src/events.rs` - the event stream pushed to the webview
- `src/updater.rs`, `src/telemetry.rs`, `src/logging.rs`, `src/panic_hook.rs`,
  `src/vss_helper.rs` - updater channel, anonymous ping, rolling file logs,
  crash capture, and the elevated VSS broker's lifecycle
- `tauri.conf.json` + `capabilities/default.json` - window, bundle, and the webview
  permission allowlist; a new plugin call needs an entry there

```sh
just dev        # cargo tauri dev (the whole app)
just bundle     # cargo tauri build
cargo test -p driven-app
```
