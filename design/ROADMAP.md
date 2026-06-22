# Driven — Implementation Roadmap

> Phased milestones from empty repo to V1 GA. Each milestone is sized
> for a single autonomous agent session (~4–8 hours of focused work).
>
> Every milestone ends with concrete **acceptance tests** the agent must
> pass before claiming completion. If the agent can't make them pass, it
> stops and surfaces the blocker rather than papering over it.

---

## Sequencing rationale

The order minimizes "implement, throw away, reimplement" churn:

1. Scaffold + plumbing first (M0), so every subsequent milestone has a
   place to live.
2. The trait seam (M1: `RemoteStore` + `InMemoryRemoteStore`) is built
   **before** the real Google Drive client, so all of M2–M5 can develop
   against the fake without burning quota or needing real OAuth.
3. The sync core (M2–M3) is built and unit-tested in isolation before any
   UI exists — that's where the hard correctness work lives.
4. Real Google Drive integration (M4) comes after the core works against
   the fake. We bolt the real impl onto a known-good seam.
5. UI (M5–M8) layers on top of a working sync engine. Each UI surface is
   its own milestone so the agent can interleave with manual review.
6. Auto-update + release pipeline (M9) and first GA tag (M10) come
   last because they need the rest to exist to ship.

---

## M0 — Scaffold

**Goal:** empty repo → workspace that builds, lints, tests (with no tests),
and bundles. The agent never has to re-bootstrap.

### Tasks
- `git init`. Set user.email = `max@maxhogan.dev`, user.name = `pmaxhogan`.
- Write `.gitignore`, `.gitattributes` (LF), `.editorconfig`, `rustfmt.toml`,
  `rust-toolchain.toml` (`channel = "stable"`).
- `Cargo.toml` workspace per SPEC §23 with empty crate stubs.
- Create each crate with a `lib.rs` containing only `//!` module doc.
- `src-tauri/` with minimal `tauri.conf.json` (per SPEC §20) and a
  `main.rs` that builds the window but doesn't do anything else.
- `ui/` with Vue 3 + Vite + TS + Pinia + Tailwind + **vue-i18n** wired
  from day one (`ui/src/i18n.ts`, `ui/src/locales/en-US.json` seeded
  with the initial app strings, `@intlify/unplugin-vue-i18n` Vite
  plugin enabled, `@intlify/vue-i18n/no-raw-text` lint rule active).
  Default `App.vue` uses `{{ t('app.welcome') }}` rather than a raw
  literal — i18n must be the default, not the retrofit.
- `src-tauri/locales/en-US.yml` seeded for tray + OS-notification
  strings; `src-tauri/src/i18n.rs` boots `rust-i18n`.
- `justfile` per SPEC §21.
- `release-please-config.json`, `.release-please-manifest.json`.
- `.github/workflows/ci.yml` per SPEC §19.1 (matrix builds + tests + lint).
- README with one-paragraph project description + "build from source"
  steps.
- License files (MIT OR Apache-2.0 dual).
- `deny.toml` (basic — disallow yanked, advisory deny, list license allowlist).
- First commit + initial CI must go green on all three OSes.

### Acceptance
- `cargo build --workspace` passes.
- `cargo test --workspace` passes (zero tests, zero failures).
- `cargo clippy --workspace -- -D warnings` passes.
- `pnpm install && pnpm build` in `ui/` passes.
- `cargo tauri build --debug --no-bundle` succeeds locally.
- CI green on Linux + macOS + Windows.

---

## M1 — Storage + RemoteStore seam + InMemoryFake

**Goal:** the entire sync engine's worldview, with no Google in sight.
Everything testable with `cargo test`.

### Tasks
- Implement `driven-core::state`:
  - SQLite via `sqlx` with offline-mode prepared queries.
  - Migrations 0001 (schema) + 0002 (settings seed) per SPEC §2.
  - `StateRepo` with typed methods (`load_source_file_state`, `upsert_file_state`,
    `enqueue_pending_op`, `query_activity`, etc.).
  - `Clock` trait + `SystemClock` + `FakeClock` (in test-fixtures).
- Implement `driven-drive::remote_store` trait per SPEC §3.
- Implement `driven-drive::fake::InMemoryRemoteStore`:
  - In-memory filesystem indexed by parent_id + name.
  - Faithful pagination, resumable session lifecycle, md5 + size.
  - Optional fault injection: `with_rate_limit_after(n)`,
    `with_network_drop()`, `with_slow_responses()`.
- Write a contract-test suite that `InMemoryRemoteStore` must pass — the
  same suite will later run against the real Google client.
- `driven-test-fixtures`:
  - `tree!()` macro for building temp directory trees.
  - `FakeClock` with `advance()` and `now_set()`.
  - `assert_remote_eq!()` helper for snapshot diffs.

### Acceptance
- `cargo test -p driven-core` passes (StateRepo CRUD round-trips).
- `cargo test -p driven-drive` passes (contract suite green against fake).
- Test for: upload + list + download round-trip, resumable upload across
  chunk boundaries, trash + list-with-trashed flag, 429-then-success retry
  contract, parallel uploads don't corrupt the fake's state.

---

## M2 — Scanner + planner

**Goal:** given a local folder and a `RemoteStore`, compute the diff plan
correctly for the full matrix of cases.

### Tasks
- `driven-core::scanner` per SPEC §6.
- `driven-core::exclude` — `.gitignore` + custom include/exclude pattern
  evaluation built on the `ignore` crate (the only confirmed crate from
  the research; v0.4.26).
- `driven-core::planner` per SPEC §7.
- Periodic deep-verify cycle: scanner accepts a "force re-hash" mode.

### Acceptance — snapshot tests in `crates/driven-core/tests/`:
- **First scan, empty remote** → plan is all uploads, no deletes.
- **Unchanged scan** → plan is empty.
- **Single file mtime change** → plan is one upload.
- **Single file deleted locally** → plan is one trash.
- **Rename** → currently produces one upload + one trash (we don't detect
  renames in V1; doc this).
- **gitignore respected** → `node_modules/foo.js` excluded.
- **`!.env` override** → `.env` included even with gitignore.
- **Exclude pattern wins** → `*.log` excluded even if not in gitignore.
- **Deep-verify catches bit-rot** → file mtime/size unchanged but
  on-disk bytes were corrupted → plan is one upload.
- **Symlink handling** — by default skipped, doc the policy.

---

## M3 — Executor + pacer + orchestrator state machine

**Goal:** the sync engine actually runs against the fake remote,
end-to-end, including retry/backoff and parallelism.

### Tasks
- `driven-core::pacer` per SPEC §9.
- `driven-core::orchestrator` per SPEC §5 / DESIGN §5.1.
- `driven-core::executor`:
  - Streaming hash + upload (blake3 + md5) per SPEC §8.
  - Small files (<5MB) → simple upload.
  - Large files (≥5MB) → resumable.
  - Resume from persisted session if found in `pending_ops`.
  - **fstat identity-check pre/post-upload** to detect file-changed-
    during-upload and atomic-replace; see SPEC §8.
  - **Crash-safe execution with `client_op_uuid` + reconciliation pass**
    per DESIGN §5.6 — kill -9 mid-upload must not produce duplicate
    Drive objects.
- `driven-core::pending_ops` reconciliation pass on startup.
- `driven-power` fake impl in `driven-test-fixtures`, plus real impls
  stubbed to return "AC always".
- `driven-core::verify` deep-verify pass.
- **Filesystem watcher per source (DESIGN §5.9)** — `notify` v8
  RecommendedWatcher, per-source event channel into the orchestrator,
  500ms debounce, exclude-prefix-aware event filtering, graceful
  degrade on inotify-limit / FSEvents-coalesce / handle-invalidation
  with surfaced activity log entries. Test: drop a file into a
  watched source → scan-tick fires within 1s; inotify limit
  exhaustion → falls back to scheduled scan + surfaces tray hint.
- **Network resilience subsystem (DESIGN §5.8)** — per-service circuit
  breakers, parallel probes (OS connectivity + captive-portal +
  per-service), reqwest `Client` per service with concrete timeouts
  per §5.8.4, connection-pool teardown after consecutive failures,
  HTTP_PROXY/HTTPS_PROXY env-var honouring. Fake-network harness in
  `driven-test-fixtures` lets a test simulate: offline, no-internet,
  captive-portal, DNS-fail, lossy, intermittent, per-service-down.

### Acceptance — integration tests in `tests/e2e_fake.rs`:
- **Fresh sync of 100 files** → all uploaded, file_state populated, no errors.
- **Sync, change 5 files, sync** → only 5 re-uploaded.
- **Sync, delete 3 files, sync** → 3 trashed on remote.
- **429 on the 7th file** → executor retries with backoff, sync completes.
- **Crash mid-upload** (drop the executor task) → next sync resumes the
  in-flight resumable session.
- **Parallel uploads** (concurrency=4) → no remote state corruption.
- **Power gate** — set fake `PowerSource` to "on battery" → orchestrator
  transitions to Paused.
- **Dry-run** → plan computed, zero remote calls executed.
- **Encryption ON** + sync round-trip + restore via direct API call →
  bytes match.
- **Concurrency / throughput** (DESIGN §11.4):
  - 1000 small files, single account: completes with default
    `min(num_cpus*2, 16)` workers; throughput ≥ 5× a serial baseline
    `parallel_uploads=1` run against the same fake remote.
  - One 1 GiB file: 3-stage pipeline keeps CPU busy during upload
    (CPU task is not idle > 90% of wall-clock).
  - Big-file hash: blake3 `update_rayon` engaged for files > 100 MiB;
    measured hash throughput ≥ 2× single-threaded blake3 baseline on
    a multi-core CI runner.
  - Adaptive parallelism: induce per-request latency increase via the
    fake remote → ThroughputProbe reduces pool size; restoring fast
    responses → pool size recovers.
  - Memory ceiling: under sustained backup of 16 in-flight 100 MB
    files, RSS stays < 400 MiB.
- **Network resilience** (fake-network harness, one test per row of
  DESIGN §5.8.1):
  - Offline → orchestrator pauses, no network calls issued.
  - Connected-no-internet → status reflects, sync pauses Drive ops.
  - DNS-fail → status reflects, no per-call hang beyond 3s.
  - Captive portal → captive-portal state surfaced + tray action fires.
  - Lossy (forced 30% packet loss, +500ms latency) → completes,
    timeouts honoured, no retry-storm.
  - Intermittent (60s up / 60s down cycles) → circuit breaker opens
    + closes correctly, queue drains when up.
  - Drive-only-down → other services still work, Drive ops pause.
  - Updater-down → sync ops continue, no update banner surfaced.
  - 5 consecutive failures → circuit breaker enters Open, next probe
    deferred per backoff schedule.

---

## M3.5 — Windows VSS for locked files

**Goal:** files held with exclusive write locks (Outlook PSTs, running
DB files, hypervisor disk images) actually get backed up — the user-named
core pain point that started the project.

### Tasks
- Win32 IVssBackupComponents bindings via the `windows` crate
  (`windows::Win32::Storage::Vss::*`). Cargo.toml: `windows = { version = "0.62", features = ["Win32_Storage_Vss", "Win32_System_Com"] }`.
- `driven-power` (or a new `driven-vss` crate) exposes a `VssSnapshot`
  RAII handle: `VssSnapshot::create(volume_letter) -> Result<Self>`,
  `snapshot.root_path() -> PathBuf` (`\\?\GLOBALROOT\Device\...`),
  drop = release.
- Orchestrator integration: per-cycle, when about to read a locked file
  (`ERROR_SHARING_VIOLATION` on first open attempt), create one
  snapshot per volume on demand, lazily, reuse for the rest of the
  cycle, release at cycle end.
- Elevation detection: on Windows-startup, query
  `OpenProcessToken` + `GetTokenInformation(TokenElevation)`. If
  not elevated, set `vss_available = false` and surface the Settings
  banner (DESIGN §5.3).
- Task Scheduler integration: a one-click "Set Driven to run elevated
  on login" action that registers a Task Scheduler entry using `schtasks
  /create` with `/RL HIGHEST`. **Immediately after registration, prompt
  "Restart Driven elevated now?"** — on Yes, calls `app.restart()` with
  the elevated entry-point so VSS is available without waiting for next
  login. On No, surfaces a banner "VSS will activate after next login".
- Settings → Rules → Windows section exposes `vss_mode` (auto / always /
  never) per SPEC §22.

### Acceptance
- Locked-file integration test on Windows: open a file with
  `CREATE_NEW | GENERIC_WRITE | 0 (no share)`, run sync, verify the
  contents land on the fake remote (read via VSS snapshot).
- Elevation-not-available test: when run un-elevated, the banner fires
  and locked files are skipped+reported (existing degrade-gracefully
  behaviour).
- Snapshot creation completes within 10s on a typical SSD volume; the
  per-cycle snapshot reuse path keeps incremental scans cheap.
- Snapshot is released on orchestrator crash / pause (via the RAII
  drop + on startup, an "orphan snapshot cleanup" sweep that releases
  any Driven-created shadows older than 1h).

---

## M3.7 - Stress harness (sister project)

**Goal:** the adversarial test surface specified in
`design/STRESS_HARNESS.md` exists and runs green on PRs. Catches the
classes of breakage the trait-seam fake cannot reach (pathological
filenames, NTFS reparse-point hazards, mid-sync mutation, kill -9
mid-pipeline, Drive-side account state changes). M3.6 was not used;
the number jump is deliberate.

> Read `design/STRESS_HARNESS.md` cover-to-cover before starting -
> the scenario catalogue is the spec. This milestone implements the
> harness binary and the hermetic + fake-drive CI jobs. The
> real-Drive job is wired but skipped until M4 lands and
> `DRIVEN_E2E_REFRESH_TOKEN` is available.

### Tasks
- New workspace crate `crates/driven-chaos/` per SPEC §1 layout and
  §23 workspace `members`. Binary crate; depends on `driven-core`,
  `driven-drive`, `driven-crypto`, `driven-power`,
  `driven-test-fixtures`. **Does not depend on `src-tauri/`** - the
  harness boots the headless core.
- Implement the `Scenario` trait, `DrivenHandle`, `CapabilitySet`
  probe, and the subcommand dispatch per STRESS_HARNESS.md §2.
- Add the new SPEC §24 error codes the harness motivates
  (STRESS_HARNESS.md §10): `local.disk_full`,
  `local.invalid_filename`, `local.ads_skipped`,
  `drive.dest_folder_missing`,
  `drive.dest_folder_permission_denied`, `harness.timeout`. Wire each
  into the appropriate crate's `thiserror` enum and the IPC
  translation per SPEC §24's pattern.
- Extend `InMemoryRemoteStore` with the fault-injection builders per
  STRESS_HARNESS.md §5 (`with_5xx_after`,
  `with_invalid_grant_after`, `with_quota_exhausted_after`,
  `with_session_invalidated_after`, `with_md5_mismatch_after`,
  `with_network_drop_after`, `with_dest_folder_missing`,
  `with_dest_folder_readonly`, `with_fileid_recycle`). These live in
  `crates/driven-drive/src/fake/fault_injection.rs` so unit tests
  can use them too.
- Implement every scenario in STRESS_HARNESS.md §3, organised by
  category module under `crates/driven-chaos/src/scenarios/`.
- Implement the filesystem and Drive-side mutators
  (STRESS_HARNESS.md §4) and the seeded `fuzz` driver.
- Implement JSON + human reports per STRESS_HARNESS.md §6.
- Cross-cutting invariant checks per STRESS_HARNESS.md §6.3 run on
  every scenario as post-conditions.
- CI jobs per STRESS_HARNESS.md §7:
  - `.github/workflows/chaos-hermetic.yml` - every PR, ~5 min, runs
    every scenario whose `requires()` doesn't need
    `cap:real_drive_creds` or Admin.
  - `.github/workflows/chaos-fake-drive.yml` - every PR, ~10 min,
    adds the fault-injection scenarios.
  - `.github/workflows/chaos-real-drive.yml` - nightly + before
    `v*` tags, gated `if: false` until M4 flips the gate.
  - `.github/workflows/chaos-soak.yml` - weekly cron, 6 h fuzz,
    opens a GitHub issue with the seed on failure.
- Add `justfile` recipes:
  ```
  chaos:
      cargo run -p driven-chaos -- scenario run-all
  chaos-fixture-clean:
      cargo run -p driven-chaos -- fixture clean --all
  chaos-fuzz:
      cargo run -p driven-chaos -- fuzz --duration 10m
  ```

### Acceptance
- `cargo run -p driven-chaos -- scenario list` prints every scenario
  in STRESS_HARNESS.md §3 with its `requires()` set.
- `cargo run -p driven-chaos -- scenario run-all` on the maintainer's
  development machine (Windows + Admin + NTFS + VSS, no real-Drive
  creds present) produces a report with: 0 FAIL, every §3.7 row
  marked `cap:real_drive_creds`-only either SKIPPED or PASS via the
  fake, every other row PASS.
- The cross-cutting invariants (STRESS_HARNESS.md §6.3) are
  evaluated on every scenario and surfaced separately from the
  scenario's own assertions.
- `chaos-hermetic` CI job passes on Linux + macOS + Windows.
  Scenarios it can't run on a given host (e.g. NTFS scenarios on
  Linux) show as SKIPPED, not FAIL, and the SKIPPED count appears on
  the PR check.
- `chaos-fake-drive` CI job passes on Linux + macOS + Windows.
- `chaos-real-drive` workflow file exists and is parseable, but the
  job is `if: false`-gated. (M4 flips the gate.)
- A deliberately-broken commit (e.g. removing the `fstat` post-check
  from `executor.rs` per SPEC §8) causes the `truncate-and-rewrite`
  and `replace-via-atomic-rename` scenarios to FAIL with a clear
  diff in the report. Verified once by the agent before claiming the
  milestone done.
- The fuzz driver, run locally for 10 minutes with a fixed seed,
  produces no FAILs and no panics. Output of one run committed under
  `design/chaos-fuzz-smoke.json` for reference.
- `driven-chaos` binary exit codes match STRESS_HARNESS.md §9:
  0 = all pass/skip, 1 = any fail, 2 = harness self-error.

---

## M4 — Real Google Drive client + OAuth

**Goal:** swap `InMemoryRemoteStore` for `GoogleDriveStore`. Same contract,
real bytes go to real Google.

### Tasks
- `driven-drive::google::oauth` PKCE loopback per SPEC §4 (using the
  `oauth2` crate, NOT `yup-oauth2`).
- `driven-drive::google::token_store` — `keyring`-backed refresh-token
  store + a thin `RefreshingTokenSource` wrapper per SPEC §4.1.
- `driven-drive::google::mod::GoogleDriveStore` implementing `RemoteStore`:
  - `reqwest::Client` with a `tower`-style middleware for retry, throttle,
    and authentication.
  - Resumable upload session protocol per
    https://developers.google.com/drive/api/guides/manage-uploads#resumable.
  - md5 verification on upload.
  - Pagination across `files.list` (`pageToken`).
  - Field selection (`fields=` query param) so we don't pull more than we
    need.
- Wire up the contract test suite to run against the real Google when
  `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID` are set.
  Uses the maintainer's own OAuth refresh token (exercises the production
  OAuth refresh code path). Each test uses a UUID-named child folder
  under the dest folder and cleans up on success/failure.
- Document the refresh-token setup in `design/E2E_REAL.md` (how to mint
  the token via `driven-cli auth` + `driven-cli dump-refresh-token`,
  how to store in `.env.test` + GH Actions secret).

### Acceptance
- Contract test suite green against the real Google when creds present.
- Manual run: agent successfully runs `cargo run --bin driven-cli auth`
  (a tiny CLI we add for end-to-end debugging without the GUI), pasting
  in its own dev-only client credentials, then `driven-cli sync`
  uploading a 3-file test folder to a real Drive folder, then verifies in
  the Drive web UI that the files appear.
- Auth re-runs on simulated `invalid_grant` (revoke + retry).

---

## M5 — Tray + boot path + autostart

**Goal:** a real Tauri app that boots into the tray, exposes pause/resume,
and starts at login.

### Tasks
- `src-tauri/src/tray.rs` per SPEC §12.
- `src-tauri/src/main.rs`:
  - `tauri_plugin_autostart` (Mac LaunchAgent, Win/Linux registry/.desktop).
  - `tauri_plugin_single_instance`.
  - `tauri_plugin_notification`.
  - Hidden main window on boot if `--minimized` flag.
  - Panic hook (SPEC §17).
- Tray menu wired to the orchestrator (sync now, pause, resume, quit).
- Tray icon swaps based on `OrchestratorState` events.
- OS notification on first-sync-completed and on error states.

### Acceptance
- App boots into tray with no visible window when launched via the
  installed bundle.
- Tray icon updates within 1s of state change.
- "Sync now" from tray triggers an orchestrator run.
- "Quit" cleanly shuts down the runtime (no orphaned tokio tasks).
- Autostart toggle in settings (next milestone) controls login startup;
  verified on Windows + macOS + Linux (GNOME, KDE).
- Second invocation of the binary surfaces the existing instance instead
  of spawning a duplicate.

---

## M6 — Settings window UI + setup wizard

**Goal:** the user can complete the BYO OAuth wizard, add accounts, add
sources, configure exclusions and rules.

### Tasks
- Vue routing per SPEC §25.
- `SetupWizard.vue` per DESIGN §8.5.
- `Settings.vue` with tabs: Accounts, Sources, Rules, About.
- `AddSourceWizard.vue`:
  - Local folder picker (`tauri-plugin-dialog`).
  - Drive folder picker (calls `pick_drive_folder` IPC, paginated tree
    view).
  - Exclusion preview ("if you run a scan now, here are the first 50
    files that will be uploaded vs excluded").
  - Encryption opt-in with recovery-phrase reveal.
- IPC commands per SPEC §11.1 / §11.2 / §11.6 fully wired.
- TS types regenerated from Rust via `cargo xtask gen-ts`.
- Pinia stores per SPEC repo layout.
- All settings round-trip through SQLite `settings` KV.

### Acceptance
- Fresh install → wizard completes a real OAuth flow against the
  maintainer's dev Google account.
- New backup source added → orchestrator picks it up on next tick.
- Encryption toggle on a source → recovery phrase shown once, master key
  written to keychain, source row updated.
- Re-auth banner appears when a refresh token is revoked (manually
  simulated by revoking in the Google Account permissions page).
- Vitest unit tests for each store + component pass.
- Playwright test boots the app and walks through the wizard against
  the fake remote.

---

## M7 — Activity dashboard

**Goal:** the user (or the developer) can see what's happening in real time
and review history.

### Tasks
- `Activity.vue` per DESIGN §8.3.
- Live tail subscribes to `activity:new` events.
- Persisted query via `query_activity` IPC + pagination.
- Filtering UI: by source, level, event type.
- "Export diagnostic bundle" button calls `export_diagnostic_bundle`
  (SPEC §18).
- Empty-state copy ("no activity yet").

### Acceptance
- Live tail updates within 500ms of an event.
- Pagination scrolls back through 1000+ events without re-querying.
- Diagnostic bundle includes everything listed in SPEC §18 and no
  secrets.

---

## M8 — Restore browser

**Goal:** in-app restore. Browse what's backed up; search by filename or
glob; restore selected files to a local folder.

### Tasks
- FTS5 virtual table + triggers (already shipped in M1 migration 0001
  per SPEC §2; M8 just exercises it from the IPC layer).
- `Restore.vue` per DESIGN §8.4.
- `list_remote_tree` IPC backed by `file_state` (avoid hitting Drive for
  navigation — we already have authoritative metadata locally).
- `search_files` IPC backed by FTS5.
- `restore_files` IPC: spawn a background job, stream `restore:progress`
  events.
- Encrypted restore path: decrypt filename for display, decrypt content
  while streaming to disk.

### Acceptance
- Browsing 10k-file tree responds within 100ms per folder open.
- Search returns within 50ms for prefix queries and within 200ms for
  glob queries.
- Restore of a 1GB encrypted file streams without holding the whole
  thing in RAM (RSS stays below 200MB during the restore).
- Restored file's blake3 matches the stored plaintext blake3.
- Cancel mid-restore cleans up the partial file.

---

## M9 — Auto-update + release pipeline (combined)

**Goal:** users get new versions automatically (stable channel) or on
opt-in (dev channel). Each new version surfaces release notes in-app.

> **Sequencing note:** auto-update and the release pipeline were
> originally split across M9 and M10 — that was a dependency reversal
> (the in-app updater needs a real `update.json` to fetch, which only
> exists once the release pipeline is in place). They're combined
> here. The maintainer-prep checklist below MUST be done before this
> milestone starts:
>
> - Generate Tauri updater ed25519 keypair (`tauri signer generate`);
>   store public key in `tauri.conf.json`, private key in GH Actions
>   secret `TAURI_SIGNING_PRIVATE_KEY` and the local maintainer
>   keychain.
> - Provision the update-manifest hosting (Cloudflare Pages on
>   `driven.maxhogan.dev/updates` — see DESIGN §9.4 / SPEC §15.3).
> - Stand up Cloudflare Worker for telemetry endpoint (see SPEC §16).
> - Confirm GitHub Actions secrets for all three OS runners.

### Tasks
- Generate the Tauri signing keypair, store private key in GH Actions
  secret + on the maintainer's machine.
- `tauri-plugin-updater` configured per SPEC §15.
- `src-tauri/src/updater.rs`:
  - Channel-aware endpoint URL.
  - Periodic check on a 6h interval, plus on startup.
  - Emits `updater:available` / `updater:downloaded` events.
- `ChangelogModal.vue` rendering the GitHub release body for the new
  version.
- Settings → About: channel toggle, "Check for updates", "View recent
  releases" pagination.
- `scripts/generate-update-json.mjs` — writes per-target update.json
  consumed by the updater.
- Hosting:
  - V0: a `gh-pages` branch with static `update.json` files.
  - V1: Cloudflare Pages on `driven.maxhogan.dev/updates`.

### Acceptance
- Manual test: build v0.1.0 locally, install it, then release v0.1.1
  via CI → installed app picks up the update, downloads, verifies
  signature, applies on next restart.
- Dev channel test: same flow with a `0.0.0-dev.<sha>` build pushed via
  `dev-channel.yml`.
- Tamper test: corrupt the bundled binary in the release asset → updater
  refuses to apply (signature check fails).
- In-app changelog shows the release notes from `CHANGELOG.md`.

---

## M10 — First GA tag

**Goal:** ship v0.1.0 to actual users.

### Tasks
- README updated with install instructions per platform (including the
  unsigned-binary bypass for Windows SmartScreen and macOS Gatekeeper —
  plus a clear note that without ad-hoc signing on macOS the in-app
  auto-updater may not work cleanly; this interaction is documented as
  a known V1 caveat or addressed per the signing decision the
  maintainer makes in the planning phase).
- `CHANGELOG.md` ratified by release-please run.
- Add `LICENSE`, `CONTRIBUTING.md` (with conventional-commits guidance),
  `CODE_OF_CONDUCT.md`.
- Tag v0.1.0.

### Acceptance
- `v0.1.0` tag creates: macOS DMG (universal), Windows MSI + NSIS, Linux
  AppImage + .deb, all uploaded to the GH Release.
- `update.json` published for both channels, all 4 platform targets.
- Maintainer's dev machine, running an older build, updates to v0.1.0
  successfully via the in-app updater (subject to the signing decision
  above).
- Anonymous telemetry from the dev machine lands in Analytics Engine.

---

## Beyond V1 (V1.1+)

Tracked as separate roadmap items, no fixed sequence yet:

- **macOS Volume Shadow Copy equivalent** (FSSnapshot via APFS clones)
  for the small subset of macOS files held with exclusive locks.
- **Schedule windows** (time-of-day rules) in the UI.
- **Pre/post backup shell hooks.**
- **rclone-crypt-format compatibility** (opt-in second format).
- **Block-level dedup (CDC)** for huge frequently-rewritten files.
- **Restore-by-date / point-in-time** (requires per-file version history
  in `file_state`).
- **Small-file bundling (tar.gz batches)** — pack dirs of many tiny files
  into a single bundle per scan to escape Drive's per-file API overhead;
  see DESIGN.md §17 for the heuristic and tradeoffs.
- **Backends beyond Google Drive** — OneDrive (Graph API), Backblaze B2,
  generic S3.
- **Web admin / status page** for users who want to monitor multiple
  machines.
- **mobile companion app** (Tauri mobile) for read-only restore.

---

## How the agent should drive itself

Per-milestone loop:

1. Read `DESIGN.md` (always — context tends to drift) + `SPEC.md`
   sections relevant to this milestone + this milestone's section.
2. `TaskCreate` one task per significant sub-step. Update as you go.
3. Implement test cases **first** for the acceptance criteria — they
   are the spec, not afterthought verification.
4. Implement the code.
5. Run `just lint && just test` to completion. Fix lint and test
   failures before claiming done.
6. Commit with a Conventional Commits message (`feat(core): ...`,
   `feat(drive): ...`).
7. Push. Watch CI go green.
8. Open a PR titled `M<n>: <milestone name>` for self-review.
9. Merge when CI is green and at least one self-review pass has been
   done (the agent should literally re-read the diff and challenge it).

If a milestone can't be completed (e.g. Google rate-limits during M4),
**do not** stub it out and claim done. Surface the blocker, document the
state in `design/BLOCKERS.md`, and stop.

When the agent has uninterrupted long sessions to spend, **M1 through
M3 can chain in a single run** — they run against the `InMemoryFake`
and `FakePowerSource`, no real network or user interaction needed.

**M4 (real Google Drive) needs a maintainer-supplied test OAuth client
and refresh token to run end-to-end.** Interactive OAuth consent
cannot be automated — the user must complete it once, in person.
Concrete preflight (do this in M0 so M4 isn't blocked):
1. Maintainer creates a dev GCP project + Drive-API-enabled OAuth
   client (Desktop type), publishes the consent screen to "In
   production" (unverified is fine).
2. Maintainer runs the wizard once on their own machine against that
   client, completing the interactive consent. The resulting refresh
   token gets exported via a `driven-cli dump-refresh-token` debug
   command (only available with `--features dev-tools`).
3. Maintainer stores the refresh token in a `.env.test` file (gitignored)
   and in a GH Actions secret `DRIVEN_E2E_REFRESH_TOKEN`.
4. From M4 onward, integration tests that need real Drive auth use
   that refresh token. The actual interactive consent path is covered
   by a small set of **manual smoke tests** the maintainer runs before
   each `v*` tag (a short checklist in `design/RELEASE_CHECKLIST.md`).

M5–M8 produce UI; the agent should request a manual UI review after
each by sending screenshots / a short Loom of the flow before
proceeding.

M9 needs the maintainer to set up the signing keypair, the Cloudflare
Pages site for `driven.maxhogan.dev/updates`, the Cloudflare Worker for
telemetry, and the GitHub repo secrets. The M9 task list opens with
that checklist.
