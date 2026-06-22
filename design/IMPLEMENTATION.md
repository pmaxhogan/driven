# Driven — Implementation Orchestration Guide

> **Read this first if you are the implementation agent.** This is the
> kickoff doc for executing the M0-M10 roadmap with `/effort ultracode`
> (xhigh reasoning + Workflow-based parallel orchestration).
>
> Planning is **done**. All design decisions are locked. There are no
> outstanding user questions blocking implementation. The four planning
> documents in this directory plus the stress harness doc are the
> complete authoritative source of truth:
>
> - `DESIGN.md` — the why + architecture
> - `SPEC.md` — the how (crate layout, schemas, IPC, code sketches)
> - `ROADMAP.md` — phased milestones M0-M10
> - `STRESS_HARNESS.md` — chaos / fuzz / soak test catalogue (M3.7)
> - `IMPLEMENTATION.md` (this file) — orchestration plan

---

## 0. Operating principle

**Parallelize wherever the work is genuinely independent. Serialize
wherever later work depends on earlier interfaces.** The roadmap is
sequential in milestones because each milestone produces capabilities
the next builds on. WITHIN each milestone there is significant
parallelism — modules touching different files, different test layers,
different platforms. That's where ultracode wins.

The pattern is consistently three phases per milestone:

1. **Interface phase** (sequential, one agent) — the shared types,
   traits, schemas, and module boundaries that downstream parallel
   work depends on. Writing one Rust module exporting types is fast
   and prevents drift later.
2. **Implementation phase** (parallel fan-out, N agents in worktrees) —
   each agent owns one module / one platform impl / one test layer.
   `isolation: 'worktree'` keeps their edits from conflicting.
3. **Integration + verify phase** (sequential, one to two agents) —
   merge worktrees into the main tree, resolve any conflicts, run
   `cargo test --workspace` + `cargo clippy` + `pnpm test`, fix
   regressions, commit.

`/effort ultracode` is on. Use Workflow for every milestone after M0.
Single-agent Agent calls are appropriate only for trivial polish.

---

## 1. Pre-flight (do this before M0)

Things that must exist before any milestone runs:

### 1.1 Repo + git
```bash
cd C:/Users/pmaxh/Documents/rust-projects/drived
git init
git config user.email max@maxhogan.dev
git config user.name pmaxhogan
```

Confirm the directory contains only `design/` and `.audit-out/` (the
audit output is informational only — add to `.gitignore`).

The remote should already exist: `github.com/pmaxhogan/driven` (public).
If not yet created, the user is expected to create it; M0 will run
`git remote add origin git@github.com:pmaxhogan/driven.git`.

### 1.2 Maintainer's dev Google OAuth credentials (for M4 onwards)

The user must complete this once on their machine before M4:

1. Create a GCP project + enable Drive API + configure OAuth consent
   (External + Published to "In production") + create a "Desktop
   application" OAuth client. (Per DESIGN §6.1.)
2. Mint a refresh token via `cargo run -p driven-app -- auth`
   (interactive). Then dump it: `cargo run -p driven-app --features
   dev-tools -- dump-refresh-token > .env.test`.
3. Add `.env.test` to `.gitignore`. Also add the same token as a GH
   Actions secret `DRIVEN_E2E_REFRESH_TOKEN`, plus a
   `DRIVEN_E2E_DEST_FOLDER_ID` pointing to a throwaway Drive folder.

M4 will check `.env.test` exists; if it doesn't, M4 will surface a
clear prompt and stop rather than silently skip its tests.

### 1.3 Tooling on the dev machine

- Rust toolchain (stable + clippy + rustfmt).
- `pnpm` (we use it instead of npm or yarn for the frontend).
- `just` (recipes from `justfile`).
- `cargo-watch`, `cargo-deny`.
- (Windows only) `windows` SDK headers for VSS in M3.5.

---

## 2. Per-milestone orchestration playbook

Each subsection below is a self-contained Workflow recipe. Use them as
templates — the agent invoking them can copy / paste the script
verbatim, change paths if needed, and run.

### M0 — Scaffold (single agent)

**Why no parallelism:** M0 is bootstrap. Every step depends on the
previous (workspace exists before crates; crates exist before lib.rs;
git exists before first commit). Trying to parallelize bootstrap
produces conflicts faster than progress.

**Run:** one general-purpose agent, no Workflow. Brief it with the
ROADMAP M0 task list + acceptance criteria. Expected wall-clock: 1-2 h.

**Critical bit:** the agent MUST write i18n scaffolding from the very
first commit per ROADMAP M0 — `App.vue` says `{{ t('app.welcome') }}`,
not a raw literal. Easier to enforce from line one than to retrofit.

### M1 — Storage + RemoteStore + InMemoryFake (3 parallel agents)

```javascript
// Workflow script for M1
export const meta = {
  name: 'm1-storage-trait-fake',
  description: 'M1: SQLite state + RemoteStore trait + InMemoryRemoteStore + test fixtures',
  phases: [
    { title: 'Interfaces' },
    { title: 'Implement' },
    { title: 'Integrate' },
  ],
}

phase('Interfaces')
const types = await agent(
  `Write the shared types in driven-core/src/types.rs based on SPEC §3
   (RemoteEntry, ResumableSession, ResumableKind, ResumeProgress,
   DownloadStream, UploadBody — note UploadBody::Stream uses
   futures::Stream<Item=Result<Bytes>> per SPEC §3), plus the StateRepo
   trait surface from SPEC §2. NO implementations — just the types,
   traits, and doc comments. Commit on completion.`,
  { label: 'types', phase: 'Interfaces' }
)

phase('Implement')
await parallel([
  () => agent(
    `Implement driven-core::state per SPEC §2: SQLite via sqlx 0.8 with
     compile-time-checked queries, migrations 0001 (schema including FTS5)
     and 0002 (settings seed) under driven-core/src/migrations/, plus
     StateRepo with typed methods. Include WAL-mode setup, integrity_check
     on startup, plus the reconciliation queries DESIGN §5.6 specifies.
     Tests for CRUD round-trips. Use the types from driven-core/src/types.rs
     (don't redefine).`,
    { label: 'storage', phase: 'Implement', isolation: 'worktree' }
  ),
  () => agent(
    `Implement driven-drive::remote_store trait + driven-drive::fake::
     InMemoryRemoteStore per SPEC §3. Faithful Drive semantics:
     duplicate-name folders allowed, file_id-based lookups, resumable
     sessions with 256 KiB chunk enforcement, appProperties storage,
     find_by_op_uuid lookup. Fault-injection extensions per
     STRESS_HARNESS.md: with_rate_limit_after, with_network_drop_after,
     with_5xx_after, with_session_invalidated_after, with_md5_mismatch_after.
     Plus the shared contract-test suite both InMemoryFake and (later)
     GoogleDriveStore must pass.`,
    { label: 'fake', phase: 'Implement', isolation: 'worktree' }
  ),
  () => agent(
    `Implement driven-test-fixtures crate: tree!() macro for building
     temp directory trees, FakeClock with advance() + now_set(),
     assert_remote_eq!() helper for snapshot diffs, plus a
     FakePowerSource and FakeNetwork harness (per DESIGN §5.8 +
     §5.7 contracts) so future tests can simulate offline,
     captive-portal, on-battery, etc.`,
    { label: 'fixtures', phase: 'Implement', isolation: 'worktree' }
  ),
])

phase('Integrate')
const integrate = await agent(
  `Merge the three M1 worktrees into the main worktree. Run
   'cargo build --workspace', 'cargo test --workspace', 'cargo clippy
   --workspace -- -D warnings'. Fix any conflicts or test failures.
   On green: 'git add -p' for each crate + commit per crate with
   Conventional Commits subjects (feat(core), feat(drive), feat(test-fixtures)).
   Push.`,
  { label: 'integrate', phase: 'Integrate', effort: 'high' }
)
```

**Why 3 parallel agents:** the three workspace crates have no
dependency cycles. Each touches different files. Worktree isolation
prevents lock contention on `Cargo.lock`.

**Expected wall-clock:** ~30 min interfaces, ~2 h parallel implement,
~30 min integrate. Total: ~3 h vs ~6-8 h serial.

### M2 — Scanner + planner (2 parallel agents)

Interface phase writes any new shared types (e.g. `ScanResult`,
`Plan`, `Op` shapes per SPEC §6-7) into `driven-core/src/types.rs`,
then two parallel agents:

- Agent A: `scanner.rs` + `exclude.rs` (gitignore + default excludes
  per DESIGN §5.2 + symlink/junction policy per §5.2.1).
- Agent B: `planner.rs` + tests.

Both depend on M1 storage. Integration agent runs the M2 acceptance
snapshot tests.

### M3 — Executor + pacer + orchestrator + watcher + network (6 parallel agents)

This is the densest milestone. Six modules, all referenced by the
orchestrator but each authorable independently against the M1 trait.

```javascript
phase('Interfaces')
const interfaces = await agent(
  `Extend driven-core/src/types.rs with: OrchestratorState (the state
   machine variants per DESIGN §5.1 + §5.6 + §5.8.6 + §5.10), Pacer
   public API (acquire methods, classify_response), PowerEvent enum
   (Suspending, Resumed) per §5.10, NetworkEvent + ServiceHealth per
   §5.8. Define the channels orchestrator <-> {power, network, watcher}
   use. NO implementations.`,
  { label: 'interfaces', phase: 'Interfaces' }
)

phase('Implement')
await parallel([
  () => agent(`Implement driven-core::pacer per SPEC §9 + DESIGN §5.4 +
              §18.1 AIMD deltas. AIMD increase/decrease + bytes_bucket
              for bandwidth cap.`,
              { label: 'pacer', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-core::executor per SPEC §8 + DESIGN §11.4.3
              pipeline (3-stage reader / cpu / uploader with bounded
              mpsc), plus the fstat identity check (pre/post lstat
              compare inode+dev), plus the client_op_uuid reconciliation
              flow per DESIGN §5.6, plus encrypted upload path.`,
              { label: 'executor', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-core::orchestrator state machine per
              DESIGN §5.1 + §5.6 + §5.8.6 + §5.10. Subscribes to
              power, network, watcher events; dispatches scan/plan/
              execute cycles.`,
              { label: 'orchestrator', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-core::watcher per DESIGN §5.9 using
              notify v8. Per-source RecommendedWatcher, 500ms
              debounce, exclude-prefix filter, graceful degrade.`,
              { label: 'watcher', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-core::network probe topology per
              DESIGN §5.8: three parallel probes (OS connectivity,
              captive-portal, per-service), AIMD-style circuit
              breakers, reqwest::Client per service with the
              §5.8.4 timeouts, connection-pool teardown after 3
              consecutive failures, HTTP_PROXY/HTTPS_PROXY honour.`,
              { label: 'network', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-power crate: PowerSource trait + per-OS
              impls (Windows GetSystemPowerStatus + WM_POWERBROADCAST,
              macOS IOPMCopyAssertionsByType + NSWorkspace sleep/wake,
              Linux /sys/class/power_supply + systemd-logind DBus for
              sleep/wake). Per DESIGN §5.7 + §5.10. FakePowerSource
              lives in driven-test-fixtures (was M1).`,
              { label: 'power', phase: 'Implement', isolation: 'worktree' }),
])

phase('Integrate')
// Merge worktrees, wire up orchestrator's event channels, run M3
// acceptance integration tests in tests/e2e_fake.rs (per ROADMAP).
const integrate = await agent(`Merge all six M3 worktrees. Wire up the
  orchestrator's input channels (power, network, watcher → orchestrator).
  Run cargo test --workspace --test e2e_fake. Fix issues. The ROADMAP
  M3 acceptance tests list (network resilience matrix, concurrency
  / throughput matrix, etc.) is the bar.`,
  { label: 'integrate', phase: 'Integrate', effort: 'high' })

phase('Verify')
// Adversarial verify pattern: a separate agent reviews the diff for
// correctness gaps the implementer might have missed.
const verify = await agent(`Adversarially review the M3 diff against
  DESIGN §5 + §11.4 + §18 (resolved defaults) + STRESS_HARNESS scenarios
  for the executor/orchestrator. Report any spec-vs-impl gaps as
  [P1]/[P2] findings.`,
  { label: 'verify', phase: 'Verify' })
```

**Expected wall-clock:** ~1 h interfaces, ~4-6 h parallel implement
(IO-bound on Drive's API mocks etc. and CPU-bound on rayon hashing),
~1-2 h integrate, ~30 min verify. **Total: ~7-10 h vs ~3-4 days serial.**

### M3.5 — Windows VSS (1 agent + bench)

Windows-only code, single-agent. The agent should be run on a Windows
worktree. Verification agent runs the M3.5 acceptance tests against a
local VHD or real volume per ROADMAP.

### M3.7 — Stress harness (per-category parallel)

`crates/driven-chaos/` per STRESS_HARNESS.md. Per the doc, scenarios
are independent — perfect fan-out per category:

```javascript
const CATEGORIES = [
  'storage-disk', 'file-size-extremes', 'permissions-acls',
  'pathological-filenames', 'ntfs-hazards', 'mutation-soak',
  'drive-side-fuckery', 'concurrency-edge',
]
await parallel(
  CATEGORIES.map(cat => () => agent(
    `Implement the ${cat} scenario category from STRESS_HARNESS.md §3.
     Each scenario implements the Scenario trait (name, requires,
     setup, run_assertions, teardown, expected_outcome). Add to
     crates/driven-chaos/src/scenarios/${cat.replaceAll('-', '_')}.rs.`,
    { label: `chaos:${cat}`, isolation: 'worktree' }
  ))
)
```

### M4 — Real Google Drive client (4 parallel agents)

```javascript
phase('Implement')
await parallel([
  () => agent(`Implement driven-drive::google::oauth (PKCE loopback +
              dual-bind v4/v6 + exact-Host validation per SPEC §4 v5
              API) + token_store (keyring-backed refresh storage) +
              RefreshingTokenSource wrapper per SPEC §4.1.`,
              { label: 'oauth', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-drive::google::mod::GoogleDriveStore
              implementing the RemoteStore trait against real
              googleapis.com.`,
              { label: 'drive-impl', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement driven-drive::google::pagination + resumable
              + retry middleware. Honors 308 Resume Incomplete,
              restarts on 4xx (DESIGN §5.4), exponential backoff with
              Retry-After per §5.4.`,
              { label: 'pagination-retry', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Wire the shared contract-test suite from M1 to run
              against the real Google when DRIVEN_E2E_REFRESH_TOKEN +
              DRIVEN_E2E_DEST_FOLDER_ID are set. Each test creates a
              UUID-named subfolder under the dest folder + cleans up.`,
              { label: 'e2e-runner', phase: 'Implement', isolation: 'worktree' }),
])
```

**Critical:** M4 requires the user's dev OAuth credentials per §1.2
above. If `.env.test` is missing, M4 surfaces a STOP with the
provisioning checklist, does NOT silently skip.

### M5 — Tray + boot path + autostart (5 parallel agents)

```javascript
phase('Implement')
await parallel([
  () => agent(`Implement src-tauri/src/tray.rs per SPEC §12 +
              DESIGN §8.1. Tray icon state machine (default/spinner/
              yellow/yellow-bang/red), menu actions, Linux-click-event
              graceful degrade (menu is canonical).`,
              { label: 'tray', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Wire tauri-plugin-autostart + tauri-plugin-single-instance
              + tauri-plugin-deep-link in main.rs in the correct order
              (single-instance FIRST per SPEC §14).`,
              { label: 'plugins', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement src-tauri/src/panic_hook.rs + diagnostic
              bundle export (SPEC §17 + §18). Redaction pipeline per
              SPEC §18.`,
              { label: 'crash-diag', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement src-tauri/src/telemetry.rs + the Cloudflare
              Worker telemetry endpoint at driven.maxhogan.dev/telemetry.
              Per SPEC §16. Opt-out default, stable install UUID,
              never auto-uploads anything else.`,
              { label: 'telemetry', phase: 'Implement', isolation: 'worktree' }),
  () => agent(`Implement src-tauri/src/i18n.rs (rust-i18n loader for
              tray + OS notifications), seed src-tauri/locales/en-US.yml
              with the M5-scope strings, plus the sys-locale detection
              + locale-changed event re-render.`,
              { label: 'i18n-boot', phase: 'Implement', isolation: 'worktree' }),
])
```

### M6 — Settings UI + setup wizard (6-8 parallel Vue components)

The frontend surfaces are mostly independent. Each Vue view + its
Pinia store can be authored in parallel, given the IPC commands are
defined first (interfaces phase):

```javascript
phase('Interfaces')
// Generate the typed IPC commands via cargo xtask gen-ts which runs
// tauri-specta. After this step, ui/src/ipc/commands.ts exists with
// typed wrappers for every backend command.
const ipc = await agent(`Define every Tauri command listed in SPEC §11
  in src-tauri/src/commands/. Add specta annotations. Run cargo xtask
  gen-ts to emit ui/src/ipc/commands.ts. Commit both backend stubs +
  generated frontend.`,
  { label: 'ipc-stubs', phase: 'Interfaces' })

phase('Implement')
await parallel([
  () => agent(`SetupWizard.vue + accompanying store + locale strings`, ...),
  () => agent(`Settings.vue: Accounts tab + AccountList component`, ...),
  () => agent(`Settings.vue: Sources tab + SourceTable + AddSourceWizard`, ...),
  () => agent(`Settings.vue: Rules tab (battery / metered / bandwidth /
              concurrent uploads / deep-verify interval — NO schedule
              windows, those are V2 per §3.5)`, ...),
  () => agent(`About.vue: version + channel selector + check-for-updates +
              release-notes pagination`, ...),
  () => agent(`Backend implementations for the M6-scope IPC commands
              (accounts/sources/settings/oauth-progress event emission)`, ...),
])
```

### M7 — Activity dashboard (3 parallel)

`Activity.vue` + its Pinia store + the backend `query_activity` /
`export_diagnostic_bundle` IPC commands. Each is independent.

### M8 — Restore browser (3 parallel)

`Restore.vue` + its store + backend `list_remote_tree` / `search_files`
/ `restore_files` IPC. The FTS5 backed search query is the only
non-trivial integration point — single agent for that.

### M9 — Auto-update + release pipeline (4 parallel, then 1 integration)

```javascript
phase('Provision') // sequential prereqs the maintainer does once
await agent(`Generate Tauri updater ed25519 keypair via 'tauri signer
  generate'. Update tauri.conf.json pubkey. Store private key in GH
  Actions secret TAURI_SIGNING_PRIVATE_KEY. Provision Cloudflare Pages
  site for driven.maxhogan.dev/updates/. Provision Cloudflare Worker
  for /telemetry/. Stand up DNS for driven.maxhogan.dev. Set GH
  Actions secrets. Output the maintainer-action checklist if any step
  needs human credentials.`,
  { label: 'provision' })

phase('Implement')
await parallel([
  () => agent(`src-tauri/src/updater.rs glue + channel-aware endpoint
              selection per SPEC §15.2.`,
              { label: 'updater-runtime', isolation: 'worktree' }),
  () => agent(`.github/workflows/release.yml + dev-channel.yml per
              SPEC §19.3 + §19.4 using tauri-action.`,
              { label: 'ci-release', isolation: 'worktree' }),
  () => agent(`.github/workflows/release-please.yml + release-please-config
              per SPEC §19.2.`,
              { label: 'ci-release-please', isolation: 'worktree' }),
  () => agent(`ChangelogModal.vue + "View recent releases" pagination in
              Settings → About per DESIGN §9.3.`,
              { label: 'changelog-ui', isolation: 'worktree' }),
  () => agent(`scripts/generate-update-json.mjs producing the per-target
              update.json artifacts published to Cloudflare Pages.`,
              { label: 'update-json-gen', isolation: 'worktree' }),
])
```

### M10 — First GA tag (1 agent + manual smoke checklist)

Single sequential milestone. Agent updates README + LICENSE +
CONTRIBUTING + CODE_OF_CONDUCT, ensures `CHANGELOG.md` is current,
tags `v0.1.0`. Then **manually walks through `design/RELEASE_CHECKLIST.md`**
(written during M10) which includes the items that genuinely can't
be automated: real OAuth interactive consent on a clean machine,
the unverified-app warning UX, macOS unsigned-bundle bypass on a
real Mac (if one is available).

---

## 3. Cross-cutting agent patterns

### 3.1 Worktree isolation

Use `isolation: 'worktree'` on every parallel-fan-out `agent()` call
that mutates source files. Without it, two agents racing to edit
`Cargo.lock` will produce nonsensical merge states.

Worktrees auto-cleanup if the agent makes no changes, so even read-
heavy agents can request worktree isolation cheaply.

### 3.2 Schema'd structured outputs

For any verification / review / findings-producing agent, pass a
JSON schema via `agent(..., { schema: ... })`. This makes the output
machine-readable for downstream synthesis. Example:

```javascript
const FINDING_SCHEMA = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          severity: { enum: ['P1', 'P2', 'nit'] },
          location: { type: 'string' },
          problem: { type: 'string' },
          fix: { type: 'string' },
        },
        required: ['severity', 'location', 'problem', 'fix'],
      },
    },
  },
  required: ['findings'],
}
```

### 3.3 Adversarial verify

After every milestone's Integrate phase, run a verify agent whose
prompt is "try to break this". Pattern:

```javascript
const verifyVotes = await parallel(
  Array.from({length: 3}, (_, i) => () => agent(
    `Adversarially review the M<N> diff against DESIGN/SPEC. Find anything
     that would silently fail in production. Report [P1] / [P2] findings.
     Default to refuted=false if uncertain.`,
    { label: `verify:${i}`, schema: FINDING_SCHEMA }
  ))
)
const realFindings = verifyVotes
  .filter(Boolean)
  .flatMap(v => v.findings)
  .filter(f => f.severity === 'P1' || f.severity === 'P2')
```

3-vote ensemble survives one verifier hallucinating.

### 3.4 Loop-until-dry for unbounded discovery

For tasks like "find all bugs" or "find all spec-vs-impl gaps" where
the count is unknown:

```javascript
const found = new Set()
let dry = 0
while (dry < 2) {
  const round = await agent(`Find spec-vs-impl gaps in the M<N> diff.
    Already-found gaps (DO NOT report again): ${[...found].join('; ')}`,
    { schema: GAP_SCHEMA })
  const fresh = round.gaps.filter(g => !found.has(g.id))
  if (!fresh.length) { dry++; continue }
  dry = 0
  fresh.forEach(g => found.add(g.id))
}
```

### 3.5 Budget-aware scaling

`/effort ultracode` removes the token constraint but the workflow
should still adapt to `budget.total` for sanity:

```javascript
const FLEET = budget.total
  ? Math.max(3, Math.floor(budget.total / 100_000))
  : 8  // ultracode default
```

---

## 4. What NOT to parallelize (anti-patterns)

- **Bootstrap (M0).** Parallel cargo-init / file-creation produces
  conflicts on `Cargo.lock`, `Cargo.toml`, `.gitignore`.
- **Migrations.** SQLite migration files are strictly ordered. One
  agent writes all of them.
- **The Cargo.toml dep table.** Two agents editing `[dependencies]`
  in the same `Cargo.toml` deadlock the merge. Designate a "dep
  curator" agent per milestone that takes the dep diffs from
  implementation agents and applies them.
- **Tauri plugin registration order.** SPEC §14 says single-instance
  MUST register first; deep-link MUST come after. Don't fan that
  out — one agent owns `src-tauri/src/main.rs`.
- **Spec / design doc edits during implementation.** If the
  implementation surfaces a needed spec change, surface it as a
  finding, not as a silent doc edit. Drift between "what the spec
  says" and "what's implemented" is a hard bug.

---

## 5. State-of-planning snapshot (post-/clear context)

**Everything you need is in the four design docs.** This section
captures the few things that exist only in the in-conversation history
prior to /clear:

- All product decisions are locked. There are NO outstanding
  AskUserQuestion-type questions blocking implementation.
- The user said the GitHub repo at `github.com/pmaxhogan/driven`
  needs to be created before M0 (or M0's first task can offer to
  create it via `gh repo create`).
- The user has explicitly opted into ultracode for the implementation
  phase. Use Workflow over single-agent Agent calls.
- The local on-disk folder is still named `drived/` (not `driven/`)
  per the user's earlier comment: "i'll handle the rename on my local
  folder later". Don't rename the directory. The Cargo project
  identifier is `driven` already.
- The `.audit-out/` directory at the project root contains the raw
  outputs of the pre-implementation audit (synthesis.md, verify-*.md,
  coherence-*.md). Informational; safe to delete or add to .gitignore.
- Maintainer git identity: `user.email max@maxhogan.dev`,
  `user.name pmaxhogan`. M0 sets these.
- License = MIT OR Apache-2.0 dual. Both LICENSE files at repo root.
- All user-facing strings flow through `t()` from the very first
  commit (no retrofit). i18n CI lint must pass on the first PR.

### 5.1 Common pitfalls already caught

The audit already caught and corrected:
- oauth2 v5 breaking API changes (SPEC §4 has the correct v5 builder pattern)
- Drive resumable session 4xx → restart-from-zero (not retry the chunk)
- Resumable upload 256 KiB chunk multiples for non-final chunks
- MD5 over ciphertext, not plaintext, for encrypted uploads
- ensure_folder duplicate-name handling via appProperties
- Tauri v2 `Emitter::emit` (NOT v1's `emit_all`)
- Tauri tray click events unsupported on Linux (menu is canonical)
- Filename encryption with XChaCha20-Poly1305 (192-bit nonce; safe with deterministic derivation), NOT raw ChaCha20-Poly1305
- age STREAM replaced with chacha20poly1305 crate's aead::stream::EncryptorBE32<XChaCha20Poly1305> directly (no age wrapper)
- yup-oauth2 explicitly NOT used; oauth2 crate v5 + custom refresh wrapper
- DRIVEN_E2E_REFRESH_TOKEN (not service-account JSON) for E2E auth
- macOS auto-update documented as "not expected to work cleanly in V1"
  due to unsigned bundles + Gatekeeper
- Sleep/wake handling section (DESIGN §5.10) for laptop suspend
- File watcher with scheduled-scan fallback (DESIGN §5.9)

### 5.2 First Workflow to run (after M0)

After M0 completes, run the M1 Workflow defined in §2 above. It will
print a final report; the maintainer reviews + commits if green.

### 5.3 Where to find...

- **Exact crate versions / dep table:** DESIGN §16.
- **SQLite schema:** SPEC §2.
- **IPC commands:** SPEC §11.
- **Error codes:** SPEC §24.
- **Resolved defaults:** DESIGN §18.
- **Settings JSON schema:** SPEC §22.
- **Tauri config:** SPEC §20.
- **CI workflows:** SPEC §19.
- **Stress harness scenarios:** STRESS_HARNESS.md §3.
- **What's deferred to V2+:** DESIGN §17.

---

## 6. After M10 — first release

Run `design/RELEASE_CHECKLIST.md` (written during M10) which covers:
- The bits that can't be automated (real OAuth consent on a clean
  machine, the unverified-app warning click-through walkthrough,
  smoke-test the in-app updater from v0.1.0 → v0.1.1).
- The maintainer-only credential rotations (if updater signing key
  ever needs to rotate).
- Post-release monitoring (telemetry first-ping verification, GH
  Issues triage).

There is no requirement to "ship V1 to users" in this session — the
exit criterion is "M10 acceptance passes and `v0.1.0` exists as a tag
+ a GH Release with signed updater payload + downloadable bundles."
Actual user acquisition is out of scope.

---

## 7. Done

When `v0.1.0` is tagged and the M10 acceptance tests pass: planning
+ implementation are complete. The maintainer takes over for actual
distribution.
