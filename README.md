# Driven

One-way local-folder backup to Google Drive. Fast, encrypted, battery-aware, with an in-app restore browser. Desktop app for Windows, macOS, and Linux, built with Tauri 2 + Vue 3 + Rust.

> **Status:** pre-alpha bootstrap (M0). The current build opens a window and renders the welcome string, nothing else. See `design/ROADMAP.md` for the milestone plan and `design/DESIGN.md` / `design/SPEC.md` for the architecture.

## Why Driven

Existing backup tools either (a) sync both ways (not what most people want for a one-way backup), (b) have no native client encryption, (c) skip files held with exclusive write locks on Windows (Outlook PSTs, running DB files, hypervisor disk images), or (d) need you to keep a separate cloud account paid up. Driven backs up your folders to your existing Google Drive, one way, fast, with optional client-side encryption, respects battery and metered networks, and on Windows uses Volume Shadow Copy so locked files actually back up.

## Build from source

Prereqs:

- Rust stable (`rustup install stable`)
- Node.js 22+ and pnpm 10+
- `cargo install tauri-cli@^2 cargo-deny cargo-watch just` (Windows users can install `just` via `scoop install just`)
- Linux build deps: `libwebkit2gtk-4.1-dev libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev javascriptcoregtk-4.1`

Clone and run:

```sh
git clone https://github.com/pmaxhogan/driven
cd driven
pnpm --dir ui install
cargo tauri dev
```

Useful recipes (see `justfile`):

```sh
just test           # cargo + vitest
just lint           # fmt + clippy + eslint
just bundle         # cargo tauri build (installers under src-tauri/target/release/bundle/)
just deny           # cargo deny check
```

## Design docs

- `design/DESIGN.md` - architecture, locked decisions, resolved defaults
- `design/SPEC.md` - concrete crate / schema / IPC / config detail
- `design/ROADMAP.md` - M0..M10 phased milestones
- `design/STRESS_HARNESS.md` - chaos / fuzz / soak test catalogue
- `design/IMPLEMENTATION.md` - implementation orchestration plan

## License

Dual-licensed under either of:

- MIT license (`LICENSE-MIT`)
- Apache License, Version 2.0 (`LICENSE-APACHE`)

at your option.

Contributions intentionally submitted for inclusion in Driven by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
