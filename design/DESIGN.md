# Driven — Design Document

> One-way desktop backup to Google Drive. Rust + Tauri v2. Free, open source, multi-platform.

This document captures **what we are building and why**. The companion `SPEC.md` is the
*how*, and `ROADMAP.md` sequences the work into milestones.

Decisions in this document were locked in 2026-06-21 via the project's initial
interactive design session and are quoted verbatim as "Locked decisions" boxes.
Anything not in a locked-decisions box is implementation discretion and may evolve.

---

## 1. Problem statement

The user backs up working data to Google Drive today via the official
**Google Drive for Desktop** client. That client has three persistent pain points:

1. **It fights actively-used files.** Opening a file in Excel / Photoshop /
   anything-with-an-exclusive-lock causes sync errors or stops the file from being
   backed up at all. The user notices this regularly.
2. **It periodically forces re-login.** Refresh tokens lapse, usually at the worst
   moment. The root cause is shipping under Google's "Testing" OAuth status,
   which caps refresh tokens at 7 days
   (https://developers.google.com/identity/protocols/oauth2/production-readiness/sensitive-scope-verification).
3. **It is not a backup tool.** It is a *sync* tool with bidirectional behaviour.
   There is no way to express "this is my source of truth — delete on the
   destination when it disappears here".

**Driven** is a small, reliable, **one-way** desktop backup client targeting Google
Drive that fixes those three pain points and adds the table-stakes a power user
wants from a backup product: encryption, multiple accounts, dry-run, scheduled and
power-aware execution, fast incremental scans, and an in-app restore browser.

---

## 2. Non-goals

These are explicitly **out of scope**, so the codebase stays focused and the
maintenance surface stays small:

- **Bidirectional sync.** Driven is one-way: local → Drive. No conflict resolution,
  no destination-wins semantics, no two-folder bisync. If Drive diverges, Drive
  loses.
- **Block-level deduplication across files.** Restic/Kopia-style CDC is not implemented.
  Each file maps to one Drive object. (Sometimes-rewritten huge files like
  Outlook PSTs will re-upload entirely — acceptable for V1.)
- **Backup to anything other than Google Drive.** The architecture has a
  `RemoteStore` trait so a future S3 or OneDrive backend is *possible*, but it
  is not planned and is not allowed to leak into V1 design tradeoffs.
- **Server / multi-user / team admin.** Driven runs on one machine for one user
  with one or more of *their* Google accounts. No web dashboard, no shared
  config service.
- **File-system mounting.** No FUSE / Dokan virtual-drive mode. Drive is the
  destination, not a virtual local path.
- **Mobile.** Desktop only: Windows, macOS, Linux. Tauri v2 supports iOS/Android
  but they are out of scope.

---

## 3. Locked decisions (from initial design session)

These are the fixed product/architecture choices. The implementation must
honour them; deviating requires re-asking the user.

### 3.1 Sync semantics
- **One-way, destination-mirrors-source.** Deletes propagate.
- **Delete behaviour:** local delete → move file on Drive to Drive's native
  trash. Google's 30-day auto-purge becomes the recovery window. No tombstones,
  no app-managed trash folder. (`rclone --drive-use-trash` default behaviour.)
- **Conflict policy:** local is always authoritative. If the Drive copy diverges
  for any reason, it is overwritten without prompting.

### 3.2 Authentication
- **Scope:** `https://www.googleapis.com/auth/drive` (full Drive). Required so
  the user can pick existing Drive folders as backup destinations.
- **OAuth client:** **BYO credentials only.** First-run wizard walks the user
  through creating their own OAuth client in their own GCP project. The app
  never ships a baked-in OAuth client. This sidesteps:
  - Google's annual sensitive-scope security audit.
  - The 7-day refresh-token expiry that bites apps in "Testing" status.
  - Per-app global rate-limit sharing.
- **Multiple Google accounts:** supported in V1. Each account has its own
  credentials, refresh token, encryption key, quota budget, and pacer.

### 3.3 Sync engine
- **State model:** **SQLite path-state**, rclone-style. One row per
  `(source_id, relative_path)` recording mtime/size/hash/drive_file_id. No
  content-addressed chunking, no CDC.
- **Hashing strategy:** mtime+size on every scan (fast path), **plus** a
  configurable periodic deep-verify cycle (default weekly) that re-hashes
  everything to catch silent corruption or filesystem timestamp lies.
- **Drive layout:** **per-source-folder destination.** When the user adds a
  local folder to back up, they pick the destination Drive folder for that
  source. Multiple sources can target different folders, accounts, or even
  shared drives.

### 3.4 UI surface
- **Tray + Settings window + Activity dashboard + Restore browser.**
- **Restore browser** includes filename / glob search (SQLite FTS over
  `file_state.relative_path`).

### 3.5 V1 feature set
- Resumable uploads (Drive resumable upload protocol).
- Bandwidth throttle (configurable MB/s ceiling).
- Metered-network awareness (skip on metered connections).
- Dry-run mode (compute plan, do not execute).
- Multiple Google accounts.
- **Client-side encryption** (Driven-native AEAD format — see §7).
- **Windows VSS** for files held with exclusive locks (Outlook PSTs,
  running DB files, hypervisor disk images) — see §5.3.
- **Filesystem watcher** for near-realtime sync — `notify` crate
  triggers an early scan within seconds of an edit; scheduled scan
  remains as a fallback so a missed event can't cause silent
  no-backup. See §5.9.
- Activity log persisted to SQLite (last 30+ days visible).
- Error notifications: tray icon state change + OS notification.
- Restore UI with filename/glob search.
- Battery awareness (no backup on battery).
- Auto-start on login.
- Auto-update from GitHub Releases (Windows + Linux full support;
  macOS see §3.6).
- In-app changelog viewer for each update.
- Opt-out anonymous telemetry + diagnostic-bundle export button.

### 3.6 Distribution
- **Code signing deferred.** V1 ships unsigned on all platforms with
  documented bypass instructions.
  - **Windows:** Microsoft Trusted Signing (~$10/mo) is the likely
    future upgrade when distribution warrants it.
  - **macOS:** the maintainer has no Apple hardware. macOS bundles
    ship unsigned and the in-app auto-updater is **not expected to
    work cleanly on macOS in V1** — macOS users do a manual reinstall
    per release. No fix planned until Apple hardware is available.
- **Update channels:** **stable + dev.** Stable = tagged releases on `main`.
  Dev = GATED dev builds (NOT every main commit): `dev-channel.yml` builds only on
  a manual `workflow_dispatch` OR when the head commit message contains the
  `[dev-build]` marker - this bounds premium CI minutes (most main commits, e.g.
  docs/refactors, have no business producing a dev installer). The dev version is
  `<next-patch>-dev.<run_number>.<sha>` (above the current stable + monotonic across
  dev builds, so the updater always offers a strictly-newer dev build). Dev is
  opt-in; stable is the default. (R9-P2-2: see design/CODEX_NOTES.md "M9 fix round 9
  (closeout)" for why this gated/versioned contract replaced the earlier
  per-commit, `0.0.0-dev.<sha>` sketch.)

### 3.7 Test strategy
- **Hybrid trait-based DriveClient.** `RemoteStore` trait; an
  `InMemoryRemoteStore` fake satisfies the contract for the vast majority of
  tests. A small handful of end-to-end tests hit real Google Drive using
  the **maintainer's own OAuth refresh token** (stored as
  `DRIVEN_E2E_REFRESH_TOKEN` in a `.env.test` file locally and as a
  GitHub Actions secret in CI), against a throwaway folder under a
  dedicated test source. This exercises the production OAuth refresh
  code path that a service-account flow would miss. Interactive consent
  cannot be automated and is covered by a manual smoke checklist
  (`design/RELEASE_CHECKLIST.md`) the maintainer runs pre-tag.
  Everything else must run hermetically and finish in under a minute on
  a developer laptop.

---

## 4. High-level architecture

```
                         ┌─────────────────────────────────────┐
                         │            Tauri shell              │
                         │   (single-instance, autostart)      │
                         ├──────────────┬──────────────────────┤
                         │   Webview    │     Backend (Rust)   │
                         │   (Vue 3)    │   tokio runtime      │
                         │              │                      │
                         │ Settings UI  │ ┌──────────────────┐ │
                         │ Activity     │ │  Sync orchestrator│ │
                         │ Restore      │ │  (state machine) │ │
                         │ Setup wizard │ └────────┬─────────┘ │
                         └─────▲────────┘          │           │
                               │ IPC               │           │
                       ┌───────┴───────────────────▼─────────┐ │
                       │              driven-core            │ │
                       │  scanner  · planner · scheduler ·   │ │
                       │  power gate · pacer · activity log  │ │
                       └─┬──────────┬────────────────┬───────┘ │
                         │          │                │         │
              ┌──────────▼──┐ ┌─────▼─────┐ ┌────────▼──────┐ │
              │ driven-     │ │ driven-   │ │  driven-      │ │
              │ drive       │ │ crypto    │ │  power        │ │
              │  RemoteStore│ │ AEAD format│ │ battery/net   │ │
              │  trait      │ │            │ │               │ │
              │  GoogleDrive│ │            │ │               │ │
              │  InMemoryFake│ │           │ │               │ │
              └─────────────┘ └───────────┘ └───────────────┘ │
                                                              │
                              SQLite (WAL mode)               │
                              ├── accounts                    │
                              ├── backup_sources              │
                              ├── file_state                  │
                              ├── pending_ops                 │
                              ├── activity_log                │
                              └── settings                    │
```

### 4.1 Process model
- **Single process** with multi-threaded tokio runtime. No separate sync
  daemon. Tauri's single-instance plugin enforces "only one driven running".
- **Backend never blocks the webview.** All scanner / uploader work happens
  on tokio tasks; the webview gets IPC events for status updates.
- **App keeps running with the window closed.** Closing the window hides it;
  Quit is only reachable via the tray menu or `--quit` flag.

### 4.2 Crate layout

| Crate                  | Responsibility                                         |
|------------------------|--------------------------------------------------------|
| `driven-core`          | Pure logic. Sync state machine, scanner, planner, exclusion rules, scheduler, activity log writer, pacer, retry/backoff. **No I/O except via injected traits.** |
| `driven-drive`         | `RemoteStore` trait + Google Drive implementation + `InMemoryRemoteStore` fake. Owns OAuth flow and refresh-token storage. |
| `driven-crypto`        | Authenticated encryption format. Filename encryption, chunked file encryption, key wrapping via OS keychain. |
| `driven-power`         | Battery + AC + metered-network detection, normalized across Windows / macOS / Linux. |
| `driven-test-fixtures` | Shared test helpers: `tempdir` fixtures, file-tree builders, fake clock, assertion helpers. |
| `driven-app` (src-tauri) | Tauri shell. IPC commands. Tray. Updater glue. Plugin wiring. Pulls the other crates together. Thin. |

The thin shell + thick core split is so the core can be exercised by
plain `cargo test --workspace` without ever booting Tauri or a webview.
Speed of iteration matters more than any micro-elegance.

---

## 5. Sync engine

### 5.1 State machine

```
                    Idle
                     │
        ┌────────────┴────────────┐
        │ tick (scheduler / fs   │
        │ event / manual)        │
        ▼                         │
    PowerCheck ── battery → Skip ─┘
        │
        ▼
    Scanning ── scan errors → ErrorPause
        │
        ▼
    Planning
        │
        ▼
    Executing ── ratelimited → Backoff ── timer ──┐
        │                                          │
        ▼                                          │
    Verifying (sample-based) ◀──────────────────  │
        │                                          │
        ▼                                          │
    Idle ──────────────────────────────────────────┘

  ErrorPause: surfaced via tray icon (red) + OS notification.
  User-initiated Pause is orthogonal — can be entered from any state.
```

Single-orchestrator-per-account. If accounts A and B both have work, two
orchestrators run concurrently, each with its own pacer + tokio task pool.

### 5.2 Scan algorithm

For each enabled backup source:

1. **Walk** local tree with the `ignore` crate's `WalkBuilder`
   (https://docs.rs/ignore — confirmed canonical). Respect `.gitignore`,
   `.ignore`, global ignore, and the source's own
   `include_patterns` / `exclude_patterns` overrides.
**Default exclude patterns** (applied to every source, AND-ed with the
source's own include/exclude + the gitignore cascade):

```
# OS noise
.DS_Store
.AppleDouble
.LSOverride
._*
Thumbs.db
ehthumbs.db
ehthumbs_vista.db
Desktop.ini
$RECYCLE.BIN/

# Editor swap / lock / temp
*.swp
*.swo
*.swn
*~
.~lock.*#
~$*

# Misc transient
*.tmp
~*.tmp
.DocumentRevisions-V100/
.Spotlight-V100/
.fseventsd/
.TemporaryItems/
.Trashes/

# VCS internals
.git/
```

`.git/` is excluded by default (toggleable, like the rest). The working
tree's files are still backed up as ordinary files, so current file contents
are preserved; what is dropped by default is the git history, branch
structure, and stashes (stashes live only inside `.git/` - the sharp edge).
Backing up only the *unpushed* objects is not feasible in a file-copy backup
model (it would need `git bundle` synthesis and git-aware restore - deferred
to a V2+ git-aware backup mode), so the default is full exclusion. A user with
local-only or unpushed repositories they care about re-includes `.git/`
per-source via `include_patterns`.

These are configurable: Settings → Rules → "Default exclude patterns"
shows them as a checked list the user can disable individually. They
exist to spare first-time users from auditing 200 lines of OS cruft
landing on Drive. They are merged with `.gitignore` (gitignore wins
where they conflict — if a user's gitignore says `!Thumbs.db`,
Thumbs.db is included).

2. **For each entry** yielded:
   - Stat: `mtime`, `size`.
   - Look up `file_state` row keyed by `(source_id, relative_path)`.
   - If no row → **new**: enqueue HashThenUpload.
   - If row exists and `(mtime, size) != (stored_mtime, stored_size)` →
     **changed**: enqueue HashThenUpload.
   - Else → **unchanged**: skip.
   - **mtime quantization caveat:** on coarse-resolution filesystems
     (FAT32 = 2s, exFAT = 10ms, network mounts of unknown precision) an
     in-place edit that preserves byte count *within the same quantum*
     can be missed. The scanner detects FS granularity at first scan
     (write-stat-write probe) and persists it per-source; when a
     filesystem has > 1s granularity, the scanner ALSO checks
     `ctime` and (where available) inode birth time, and falls back to
     hashing any file whose mtime falls within one granularity window of
     the last-scan-end timestamp.
3. **Detect deletions** *safely*: a `file_state` row whose path the
   walker did not yield is enqueued for Delete **only if the entire
   subtree-walk that should have visited it completed without error**.
   If any directory under the source root errored during walking (the
   `ignore::Walk` iterator surfaced an `Err` for that path or any of
   its ancestors), we suppress delete propagation for that subtree and
   re-scan next cycle. A permission denial on `~/Documents/Foo/` must
   NEVER cascade to "delete everything under Foo on Drive". This
   means: do not use `filter_map(Result::ok)` to silently swallow walker
   errors. Errors are logged to the activity log; the scan reports a
   `WalkError` partial-success status to the orchestrator.
4. **Periodic deep-verify:** if `now - last_full_hash_at > deep_verify_interval`
   (default 7 days), re-hash every file regardless of mtime/size. If the
   computed hash differs from the stored hash, enqueue HashThenUpload.

The scan is intentionally streamed: we don't materialize the whole tree
before queuing work. Memory stays bounded.

#### 5.2.1 Symlinks, hardlinks, junctions, reparse points

Default V1 policy (each per-source overridable):

- **Symbolic links** → **not followed**. The link itself is not backed up.
  Rationale: following can recursively walk out of the configured
  source (e.g. into `/`), can cause infinite loops, and the user
  almost certainly didn't intend to back up "the destination of every
  symlink under here". Documented as a default with a per-source
  "follow symlinks" toggle.
- **Hardlinks** → each visible path is backed up independently. We do
  **not** detect that two paths share an inode. This means the bytes
  may be uploaded twice. Worth the simplicity for V1.
- **Windows junctions / mount points** → skipped (treated as a kind of
  symlink). Reparse points other than symlinks/junctions (Dedup, OneDrive
  placeholders, etc.) are read as the file would be opened normally;
  the OS handles the indirection. OneDrive Files-On-Demand
  (`FILE_ATTRIBUTE_RECALL_ON_OPEN`) explicitly triggers a download —
  the scanner detects this attribute and **skips by default** (don't
  silently pull down terabytes of OneDrive cloud-only files), surfaced
  per-source as "include cloud-only OneDrive files" toggle.
- **Cycles** → walker honours its built-in cycle detection (loop limit
  + visited-inode set when following).

#### 5.2.2 Overlapping / nested sources

Two backup sources whose local paths overlap (one is an ancestor of the
other) are **rejected** at add time with a clear error: "Folder A is
already inside folder B, which is also being backed up. Pick one or
the other, or split them." Reasoning: deduplicating uploads across
overlapping sources is doable but the UX gets confusing fast, and we
have no use case for it. V2 may revisit.

#### 5.2.3 Unicode normalization & case

The path-state primary key is `(source_id, relative_path)`. **`relative_path`
is stored Normalisation-Form-C (NFC) regardless of the local
filesystem's native normalisation.** Without this, the same file on
macOS (NFD) and Windows (NFC) appears as two different rows after
restore. The scanner normalises every path to NFC before lookup and
storage. A collision detector flags cases where two distinct local
paths normalise to the same NFC string (rare but possible on case-
sensitive filesystems with mixed-form names) and surfaces an error
rather than silently dropping one.

Case sensitivity: on case-insensitive local filesystems (NTFS default,
APFS default) a single file maps to one row. On case-sensitive
filesystems (ext4, NTFS-with-flag, APFS-case-sensitive) two files
differing only in case map to two rows. Drive permits two files with
the same name in the same folder, so this is representable on the
remote — we use `drive_file_id` as the actual key on the Drive side,
not the name.

### 5.3 Open-file handling

Why Google Drive Desktop fails: it opens files without `FILE_SHARE_DELETE`
and with default Windows share modes that block other writers.

Driven reads files like this:

- **Windows:** `OpenOptionsExt::share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE
  | FILE_SHARE_DELETE)` so Word / Excel / Photoshop can still write to,
  rename, or delete the file while we read.
- **macOS / Linux:** standard `std::fs::File::open` — POSIX advisory locks
  don't block reads in practice.
- **Hard-locked file** (Outlook PSTs, BitLocker images, OS swap files,
  hypervisor disk images): if `open()` returns `ERROR_SHARING_VIOLATION`,
  Driven falls through to **VSS (Volume Shadow Copy Service)** on
  Windows:
  - Each backup source whose root resolves to a Windows volume gets a
    per-cycle VSS snapshot at the start of the cycle (`vssadmin create
    shadow /for=<vol>` via the Win32 IVssBackupComponents API; the
    `windows` crate covers the COM bindings).
  - The snapshot is mounted to a per-cycle path (`\\?\GLOBALROOT\...`)
    that we use as the effective read root for the locked files only —
    unlocked files are still read from the live tree to avoid
    snapshot-creation overhead per file.
  - The snapshot is released at end-of-cycle. Snapshot creation needs
    Administrator privileges; on first run Driven detects whether
    elevation is available and, if not, surfaces a one-time Settings
    banner "To back up files Outlook / SQL / hypervisor disk images
    hold open, Driven needs to run elevated to use Volume Shadow Copy.
    Right-click → Run as administrator. Or click here to add a Task
    Scheduler entry that runs Driven elevated on login." Without
    elevation we degrade gracefully: skip locked files exactly as we
    do today and surface the count in the activity log.
- **macOS / Linux** have no equivalent of VSS that fits a backup
  client's model. On macOS, file coordination + APFS clones could help
  in narrow cases (V2+ research). On Linux, btrfs/ZFS snapshots would
  require ops the user must already have set up; we don't try.

#### 5.3.1 Least-privilege VSS helper (V1.x, issue #25)

VSS snapshot creation needs Administrator rights. The pre-V1 approach
elevated the WHOLE app (a `schtasks /RL HIGHEST` logon task) - removed
2026-06-25 because running the entire backup client, its OAuth tokens,
its network stack, and its Drive credentials as Administrator is a poor
least-privilege posture. The replacement elevates ONLY the shadow-copy
operation via a small privileged helper; the main app stays un-elevated.

**Trust boundary.** The un-elevated main app is UNTRUSTED by the helper.
The helper is the sole holder of Administrator rights and is the security
authority: it decides what may be snapshotted and what bytes flow back.
Everything crossing the boundary (a request naming a volume + a file
path) is untrusted input the helper re-validates from scratch.

**Process model.** An on-demand elevated broker, NOT a persistent SYSTEM
service. The main app launches `driven-vss-helper.exe` elevated
(`ShellExecuteW` `runas` verb -> one UAC prompt) when locked-file backup
is first enabled; the helper lives only for the app session and exits
when the app disconnects and asks it to (`Shutdown`) or when the app
process dies. This keeps no always-on elevated attack surface on the box
and needs no installer. (A one-time-install Windows service is a
documented future alternative for silent operation; the same IPC + auth
+ validation contract applies unchanged.)

**IPC surface.** A Windows named pipe `\\.\pipe\driven-vss-<random>`
(the random suffix is generated by the app and passed to the helper on
its command line, so the pipe name is unguessable). Framing is a
length-prefixed frame with a 1-byte kind (`0` = JSON control message,
`1` = raw data chunk), so streamed file bytes carry no base64 overhead
and control frames stay small + bounded. The control vocabulary is
deliberately tiny:
- `Hello { protocol_version }` / `HelloOk` - version handshake.
- `OpenLocked { volume, live_path }` -> `OpenOk { size }` or `Error` -
  create/reuse the volume's snapshot and open the mapped locked file.
- `Read { max_len }` -> a single `kind=1` data frame (empty = EOF) -
  the client pulls the file's bytes; the helper streams from the shadow
  copy so the un-elevated app NEVER opens the `\\?\GLOBALROOT` device
  itself (it cannot - the shadow device is not readable un-elevated;
  streaming through the helper is what makes least-privilege work).
- `CloseFile` -> `Ok`; `EndCycle` -> release all snapshots (per-cycle
  reuse mirrors §5.3); `Shutdown` -> release + exit.
One locked file per connection; the helper accepts many concurrent
connections (an executor worker pool backs up several files at once) and
shares ONE snapshot per volume across them under a mutex, exactly the
per-cycle-reuse contract the in-process `RealVssProvider` uses.

**Client authentication (helper trusts nobody by default).** Two layers:
1. The pipe's security descriptor grants access to ONLY the creating
   user's SID plus `BUILTIN\Administrators` and denies everyone else, so
   another user's process cannot connect at all.
2. On each accepted connection the helper reads the client's PID
   (`GetNamedPipeClientProcessId`), resolves its full image path
   (`QueryFullProcessImageNameW`), and requires it to live in the helper's
   own install directory - so a different same-user process cannot drive
   the helper. (Signature verification is a documented hardening
   follow-up.)

**Server authentication (app trusts nobody either).** The client
verifies the server end is actually the expected `driven-vss-helper.exe`
in the same install directory before sending any path, so a rogue
same-name pipe squatting cannot impersonate the helper.

**Input validation on the boundary.** Every `OpenLocked` request is
checked before any COM call: the volume must normalise to a real
`X:` drive; the `live_path` is canonicalised and must resolve UNDER one
of the configured Driven source roots the app passed to the helper at
launch (no `..` escape, no symlink escape, no arbitrary system path) -
so a compromised main app can only ever read files the user already
told Driven to back up, never `C:\Windows\...` or another user's data.
Frame sizes are capped.

**App-side wiring.** A `BrokeredVssProvider` implements the same
`VssProvider` seam the executor already consults (§5.3): its
`map_for_volume` streams the locked file's bytes from the helper into a
short-lived temp file the un-elevated app owns and returns THAT path, so
the executor's existing `read_path` open/identity/encrypt/upload
pipeline is unchanged. Temp copies are deleted at `end_cycle`. The
helper owns the real shadow copies under a `VSS_CTX_BACKUP` (auto-release)
context bounded by the helper process lifetime, so orphan cleanup needs
no cross-process ledger - a helper crash leaves only an OS-auto-reclaimed
shadow.

**Degrade + Settings banner.** When locked-file backup is degraded
because the helper is unavailable (not launched, UAC declined, off
Windows), the executor skips locked files exactly as the un-elevated
path does today and surfaces `local.vss_unavailable`; the Settings screen
shows a one-time banner offering to enable locked-file backup (which
triggers the elevated helper launch). The `windows.vss_helper` boolean
setting gates whether the brokered helper path is used at all (default
off: the historical launch-elevated-manually behaviour is preserved).

### 5.4 Upload pipeline

Per-account:

- **Worker pool:** per-account `UploadPool` permits — see §11.4.2 for
  the canonical default and rationale (`min(available_parallelism * 2, 16)`).
- **File-size thresholds (canonical, referenced elsewhere by these names):**
  - `RESUMABLE_THRESHOLD = 5 MiB` — files at or above this go through
    Drive's resumable upload protocol; below uses simple multipart.
  - `PIPELINE_THRESHOLD = 4 MiB` — files at or above this use the
    3-stage producer/consumer pipeline (§11.4.3); below run inline in
    one task (pipeline overhead would dominate).
  - `RAYON_HASH_THRESHOLD = 100 MiB` — files at or above this trigger
    `blake3::Hasher::update_rayon` for multi-core hashing inside the
    CPU stage of the pipeline (§11.4.4); below use single-threaded
    hashing.
- **Pacer:** adaptive token bucket. Drive's modern quota system uses
  per-method **quota units** (see published list at
  https://developers.google.com/drive/api/guides/limits — typical
  values: read = 5 units, list = 100, download = 200, edit = 50, other
  = 5), enforced per-user-per-100s and per-user-per-day. The exact
  numeric ceilings move and are deliberately not published as hard
  numbers, so the pacer **probes** rather than hard-coding a
  queries/sec value:
  - Starts optimistic at ~50 quota-units/sec sustained, ~100 burst (well
    above rclone's ultra-conservative defaults but conservative against
    the typical per-user daily ceiling).
  - Tracks each request's unit cost (looked up from the published table
    per request kind).
  - On any `403 userRateLimitExceeded` / `403 rateLimitExceeded` /
    `429`: halve the per-second budget AND apply exponential backoff
    with jitter (the response often carries a `Retry-After` header —
    honour it). Retry indefinitely (the limit is recoverable, never
    terminal).
  - On a sustained 10-minute clean window: raise the budget additively
    (AIMD). Saved per-account so each account learns its own ceiling.
  - On `403 dailyLimitExceeded` / `403 quotaExceeded` (the harder cap):
    pause the account until midnight Pacific (Drive's quota reset
    boundary) + surface "Daily quota exhausted" to the user. Don't
    keep hammering.
  - Per-account state. Hard cap on the budget configurable.

  Why no fixed published number: Google has explicitly moved away from
  publishing the per-user-per-100s ceiling because they reserve the
  right to tune it. The right design is probe-and-adapt, not "hard-code
  X queries/sec".
- **Resumable uploads** for files > 5 MB (Drive's threshold). Smaller files
  go via simple multipart upload to save a round-trip. Resumable session
  URLs survive process restarts: persisted in `pending_ops.payload_json`.
- **Hashing on-read:** blake3 (for the local state, computed over
  *plaintext* bytes — change detection key) and md5 (for Drive's
  `md5Checksum` metadata, computed over the *exact bytes sent to Drive*
  — ciphertext when the source is encrypted, plaintext otherwise). The
  two hashers tee off different points in the pipeline: blake3 from
  the file reader, md5 from the upload-body stream after any encryption
  wrap. blake3 because it's roughly an order of magnitude faster than
  sha256 on modern CPUs; md5 because Drive returns it for us to verify
  the upload landed intact.
- **Verification:** after upload, compare the `md5Checksum` Drive returns
  to the one we computed. Mismatch → retry. Three consecutive mismatches
  on the same file → mark `status='corrupt'`, log, surface to user.
- **Retry semantics:**
  - `429 Too Many Requests` / `403 rateLimitExceeded` / `403
    userRateLimitExceeded` → exponential backoff with jitter (1s, 2s, 4s,
    8s, capped 60s, honour `Retry-After`), retry indefinitely.
  - `5xx` → exponential backoff, max 6 retries.
  - `400 invalidGrant` / `401 invalidCredentials` → mark account
    `needs_reauth`, stop the orchestrator for this account, OS notify.
  - Other `4xx` → fail this op, log error, continue with the rest of the
    queue.
  - **`4xx` during an in-flight resumable upload** is a special case:
    per Drive's spec, any 4xx (other than 308 Resume Incomplete) on a
    resumable session means the session is terminated and must be
    recreated from scratch. We discard the persisted session URL,
    re-create with `resumable_session(...)`, and re-upload from
    byte 0. (The original session's partial bytes get garbage-collected
    by Drive after the session expires.) **Never** issue a fresh
    `resume_chunk` against the old session URL after a 4xx, even
    transient ones, even 410 — Drive will keep returning 4xx and the
    upload will be stuck.

### 5.5 Delete propagation

After scan, the deletion plan is the set of `file_state` rows whose path
the walker didn't yield (and which weren't excluded by the source's
current ignore patterns — pattern changes are not interpreted as deletes).

Each delete is a `PATCH /files/{id}` with `{"trashed": true}`. We do
**not** use `DELETE /files/{id}` (permanent) — Drive's 30-day trash is the
recovery window.

Deletes share the pacer with uploads but go into a separate priority lane
so a huge upload backlog doesn't stall delete propagation.

**Ignore-pattern changes do NOT delete.** When a user changes a source's
include/exclude patterns or toggles `respect_gitignore`, files that were
previously backed up but are now excluded by the new patterns are NOT
auto-deleted from Drive. The reasoning: pattern changes are configuration,
not deletion intent. We mark the affected rows `status='excluded_orphan'`
in `file_state`, and the Activity dashboard surfaces a banner
("234 files on Drive are no longer covered by your patterns — keep them
on Drive / move to Driven trash / permanently delete?") with explicit user
action. The implicit-deletion path is reserved for *actual local file
deletions* only.

### 5.5.1 Point-in-time versioning (trash-as-version-store, issue #36)

Opt-in per source. When a source has versioning ENABLED, a content change to
an already-uploaded file is applied as a CREATE of a NEW Drive object followed
by an atomic pointer flip and a trash of the OLD object, instead of the default
in-place `update` (PATCH). The OLD object survives - retrievable by id from
Drive's trash - as a prior VERSION, so the user can "restore as of <date>".

Mechanism (executor `upload_and_commit`):
- The change routes through the existing, crash-safe CREATE path (fresh
  `client_op_uuid`; `pending_ops.payload_json.drive_file_id` stays `null` so
  reconciliation treats it as a pure create). The old id rides in a new
  additive payload field `supersedes_drive_file_id`.
- On success, `commit_versioned_create_result` runs ONE transaction: insert a
  `file_versions` row for the OLD object (`created_at` = the old
  `last_uploaded_at`, `superseded_at` = now), upsert `file_state` to the NEW
  object (the atomic pointer flip), delete the finalizing `upload` op.
- The OLD object is then best-effort trashed (guarded by a GLOBAL "no live
  `file_state` pointer references this id" check, so a shared object - e.g. a
  future small-file bundle - is never trashed); a reconcile startup sweep
  retries any left un-trashed.
- Cost is bounded per source: a **count cap** (`versions_over_cap` +
  `delete_permanent` hard-deletes the oldest excess, also guarded) and an
  optional **size guard** (`max_bytes`; a larger old object falls back to an
  in-place update). Config lives in the `settings` KV under
  `versioning:<source_id>` - additive, no `backup_sources` schema change.
- An identical-content touch (new plaintext BLAKE3 == old) keeps the old object
  and trashes the redundant new one WITHOUT recording a version, so a repeated
  mtime-only touch cannot evict real history via the cap.

Data-format note (post-1.0): additive `file_versions` table (migration 0006)
plus the KV config; versioning is OFF by default, so existing installs are
byte-for-byte unchanged. This is a `feat` (minor), not a `feat!`.

Restore-by-date (`restore_files(..., as_of)`): per selected file, if the current
bytes were already in place at `as_of` restore them, else resolve the
`file_versions` row whose `[created_at, superseded_at)` window contains `as_of`
and download THAT object (Drive downloads work on trashed objects), verifying
against the version's plaintext BLAKE3 + size. A missing version (predates
versioning, size-guarded, pruned, or purged from trash) rejects the job with a
clear message; a version whose object was purged surfaces the normal per-file
download error. **Limitations (documented follow-ups):** the version store is
bounded by Drive's ~30-day trash retention (and a manual "empty trash" wipes
it); and files that were *deleted* locally are out of scope for point-in-time
restore in this slice (the tree browses current `file_state` only) - restoring
a since-deleted file as of a past date is the primary follow-up.

### 5.6 Crash-safe execution & reconciliation

The hard problem: a `pending_op` row + an external Drive create are two
separate state changes that we cannot atomically commit. Crash after the
Drive POST returns but before the row updates → on next start we would
re-create the same file, leaving an orphan duplicate on Drive forever.

The protocol:

1. **Before** issuing any create or update, the executor writes a
   `client_op_uuid` (v4) into the `pending_ops.payload_json`, transactionally.
2. **The create/update is sent with this UUID in `appProperties.driven.client_op_uuid`.**
   Drive stores it atomically with the file.
3. **After** the API returns, the executor writes the resulting `file_id`
   into `file_state` and clears `pending_ops`, transactionally.
4. **On crash recovery (startup):** for every still-pending `op` with a
   `client_op_uuid`:
   - If it's an `update`, look up by the existing `drive_file_id` and
     compare `appProperties.client_op_uuid`. If matches → already committed,
     mark op done.
   - If it's a `create`, call `find_by_op_uuid(parent, uuid)`:
     - Found → adopt the file_id, mark op done.
     - Not found → the create never completed; re-run normally.
   - If duplicate found (UUID collision is astronomically rare but a
     duplicate is possible if a bug repeated the create with a different
     UUID) → adopt the most-recent, trash the older, log.

This makes the executor genuinely safe to `kill -9`. The reconciliation
pass on startup is cheap (only touches rows in `pending_ops`, not every
file).

### 5.7 Power & network gates

Before starting a new batch of work (or every minute mid-batch), check:

- **AC power:** if on battery → pause. Resume when AC connects (event-driven
  on Windows via `WM_POWERBROADCAST`, polled at 30s elsewhere as a fallback).
- **Metered network:** if metered → pause. Detected via:
  - Windows: `INetworkCostManager::GetCost`
    (`NLM_CONNECTION_COST_FIXED` or `_VARIABLE` → metered).
  - macOS: `NWPath.isExpensive`.
  - Linux: NetworkManager's `Metered` property on the active connection.
- **Network reachability:** see §5.8 below — this is its own subsystem,
  not just a boolean.
- **Manual pause:** orthogonal, persists across restarts.
- **Schedule windows (optional, V2):** time-of-day gating.

### 5.8 Network resilience

Networks fail in many distinct ways. Driven must distinguish them, behave
correctly for each, and never spin pointlessly. The principle: **probe
the specific dependency that's about to be used, with a short timeout, a
clear failure classification, and circuit-breaker behaviour** — don't
rely on a single "am I online?" check.

#### 5.8.1 Failure modes & expected behaviour

| Failure mode                              | Detection                                                                  | Driven behaviour                                                                                                |
|-------------------------------------------|----------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------|
| **Airplane mode / rfkill / no interface** | OS API (Windows `INetworkListManager`, macOS `NWPathMonitor`, Linux `NetworkManager`'s `state`) reports no active connectivity | Pause sync. Tray icon yellow. Status "Offline". Re-probe on OS event "interface up"; no polling.                |
| **Connected but no Internet** (link-local only) | Dual probe to Google's `gen_204` AND our `driven.maxhogan.dev/updates/_health` → both unreachable | Pause sync. Status "Connected to network, no Internet". Re-probe every 30s.                                     |
| **DNS broken**                            | `tokio::net::lookup_host` for our probe domains returns `ResolutionFailed` within 3s timeout, OR returns NXDOMAIN for known-good domain | Pause sync. Surface "DNS not resolving" to user. Re-probe every 60s. **Do not cache failed resolves** — re-resolve every retry. |
| **No IPv4** / **no IPv6**                 | Probe attempts on both AF_INET and AF_INET6 sockets; track per-AF success rates | If one AF works, prefer it. If both fail, treat as no-Internet. `reqwest` is configured with `happy_eyeballs` so a slow AF doesn't block. |
| **Lossy / high-latency** (99% loss, 10s+) | Per-request: connect timeout 10s, request timeout 60s for small calls, no overall cap for resumable uploads (those handle their own progress monitoring) | Backoff per-request. Circuit-breaker (§5.8.3) trips after N consecutive timeouts; status reflects "Network unstable". |
| **Modern captive portal** (HTTPS-aware)   | Probe known-portal-detection endpoints: GET `http://www.gstatic.com/generate_204` expects 204 with empty body. Any non-204 / non-empty / different host → captive portal. (We use HTTP, not HTTPS, because captive portals intercept HTTP cleanly; HTTPS to known endpoints would error on the cert which doesn't help us.) | Pause sync. Surface a tray notification with a "Sign in to network" action that opens the portal URL in the user's browser. Re-probe every 30s. |
| **Poor / broken captive portal**          | Generate-204 probe returns a redirect to an HTTP URL but the redirect target is unreachable or serves garbage | Treat as "captive portal, may need manual intervention". Same UI as above but tooltip "Captive portal detected but couldn't auto-open". |
| **Multi-adapter** (ethernet + wifi etc.)  | `reqwest`/OS picks routing automatically. We don't bind to a specific adapter | No special handling — the OS routes. We do log the active adapter family in diagnostics for support cases. |
| **Intermittent** (10s-of-minutes flapping) | Circuit-breaker (§5.8.3) trips and recovers. Probe interval backs off when consistently failing. | Don't burst-retry on every blip. Drain queue when stable for >30s.                                              |
| **Drive API down** (everything else works) | Drive probe (`GET /drive/v3/about?fields=user`) fails with 5xx/timeout while update probe succeeds | Pause Drive ops only. Surface "Google Drive unavailable" (link to https://status.cloud.google.com/). Telemetry + auto-update keep working. |
| **Our update endpoint down**              | `driven.maxhogan.dev/updates` 5xx or unreachable                                    | Skip update check this cycle. Never blocks sync. Surface "Update check failed" only after 24h of failures.      |
| **GitHub releases down** (for changelog)  | api.github.com returns 5xx                                                 | Skip release-notes fetch. In-app changelog shows "Release notes unavailable — try later or open on github.com". Never blocks anything. |
| **Telemetry endpoint down**               | Cloudflare Worker returns 5xx or unreachable                               | Drop the ping silently. Never retry-burst, never block, never surface to user.                                  |

#### 5.8.2 Probe topology

Three probes, run independently, all with **3s connect / 5s total
timeout** and **DNS re-resolved each call**:

1. **OS-level connectivity** — `NWPathMonitor` (macOS),
   `INetworkListManager::IsConnectedToInternet` (Windows),
   NetworkManager `Connectivity` enum (Linux). Cheapest. The starting
   point: if the OS says "no", we don't bother probing further.
2. **Captive-portal detection** — `http://www.gstatic.com/generate_204`.
   If the OS reports connected but this returns anything other than
   204 with empty body, we're behind a captive portal.
3. **Service-specific probes** — one per service we depend on:
   - Drive: `GET /drive/v3/about?fields=user&access_token=...` (1
     quota unit, cheap; only runs when we already have a valid
     access token, otherwise skip)
   - Update endpoint: `HEAD https://driven.maxhogan.dev/updates/_health`
   - Telemetry: best-effort, never probed (just send and forget)

All three run **in parallel** during a connectivity check. The
combined result determines what's available; the orchestrator pauses
ops whose service is down and continues ops whose service is up.

#### 5.8.3 Circuit-breaker pattern

Per-service (Drive, update endpoint, etc.), maintain a small state
machine:

- **Closed** (healthy) — requests flow normally.
- **Open** (failing) — entered after **5 consecutive failures**.
  Subsequent requests fail-fast without hitting the network, with a
  small `Backoff` delay enforced by the orchestrator. Probe attempts
  run on an exponential schedule: 30s, 1m, 2m, 5m, 10m, then plateau
  at 10m.
- **Half-open** (probing recovery) — single probe request after the
  backoff. Success → Closed. Failure → Open with extended backoff.

This caps wasted work during outages and prevents the "10,000-queue-
items-each-time-out-after-60s" spiral.

#### 5.8.4 Timeouts (concrete)

| Operation kind                          | Connect | Total request | Idle (between bytes) |
|-----------------------------------------|---------|---------------|----------------------|
| OS connectivity probe                   | 3s      | 5s            | —                    |
| Captive-portal probe                    | 3s      | 5s            | —                    |
| Drive metadata (about, list, get)       | 10s     | 30s           | 10s                  |
| Drive simple upload (≤5MB)              | 10s     | 60s           | 30s                  |
| Drive resumable session create / commit | 10s     | 30s           | 10s                  |
| Drive resumable chunk PUT               | 10s     | none*         | 30s                  |
| Update manifest fetch                   | 10s     | 15s           | 5s                   |
| Update binary download                  | 10s     | none*         | 60s                  |
| Release-notes fetch (github.com)        | 10s     | 15s           | 5s                   |
| Telemetry ping                          | 5s      | 10s           | —                    |

`*` resumable chunks and binary downloads have no overall cap — they
can be arbitrarily large — but the per-chunk idle timeout catches
truly-stuck transfers.

`reqwest::Client` is built once per service with these timeouts baked
in (the per-call API doesn't let us vary them granularly otherwise).

#### 5.8.5 Connection hygiene

- `reqwest` HTTP/2 with connection pooling; max idle per host = 4,
  idle timeout = 90s. Long-idle connections get force-closed via the
  underlying `hyper`'s pool — guards against the "ISP silently dropped
  my TCP session" failure where the next request hangs 60s before
  RSTing.
- DNS: NEVER cache resolved IPs at the application layer. Let the
  OS's resolver cache (which honours TTL) be the only cache. Custom
  resolution via `hickory-resolver` if we discover OS resolver
  pathologies in the field.
- After **3 consecutive request failures on the same connection**
  (network-level errors, not HTTP 4xx/5xx), discard the entire
  connection pool for that service. The next request opens fresh.

#### 5.8.6 What this means for the orchestrator

The orchestrator's state machine (§5.1) gains explicit
network-aware substates:

- `Idle.NetworkOffline { since }` — OS says no connectivity.
- `Idle.NoInternet { since }` — connected, generate-204 fails.
- `Idle.CaptivePortal { url }` — captive portal detected; tray action
  available.
- `Backoff.ServiceOpen { service, until }` — circuit-breaker open for
  one specific service.
- `Executing.Degraded { ok_services, down_services }` — partial
  service availability; we run what we can.

Each state has a concrete UI representation in the tray + Activity
dashboard. The user is never left wondering "is it stuck or working?".

#### 5.8.7 What we don't try to do

- **Detect & route around per-route IP-blacklisting.** If Google Drive
  is blocked at the network layer (corporate firewall, regional
  block), we treat that as "Drive unreachable" — Driven is not a VPN.
- **Proxy autodetection (PAC).** V1 honours `HTTP_PROXY`/`HTTPS_PROXY`
  env vars via `reqwest`'s built-in support; PAC and SOCKS are V2.
- **mTLS / corporate-CA bundles.** V1 uses the OS's trust store via
  `rustls-native-certs`; pinning a corporate CA bundle is V2.

### 5.9 Filesystem watcher

Pure-scheduled sync (every N minutes) is a bad UX for the "saved a
file 30 seconds ago, where is it on Drive?" case. V1 adds a
**filesystem watcher per source** that triggers an early scan within
seconds of an edit. The scheduled scan remains the authoritative
fallback so a missed watcher event cannot cause silent no-backup.

#### 5.9.1 Architecture

- Crate: `notify` v8 — backends inotify (Linux), FSEvents (macOS),
  ReadDirectoryChangesW (Windows).
- One `RecommendedWatcher` per backup source, watching the source's
  local_path recursively.
- Events flow into a per-source `tokio::sync::mpsc::Sender<Event>`
  consumed by the orchestrator.
- The orchestrator treats a watcher event as a **scan-tick request**,
  not as a per-file upload trigger. Reason: events arrive in bursts
  (an editor's save = open-write-rename-fsync = 4+ events on the same
  file), and the scanner is the authoritative diff-and-plan engine.
  Watcher saying "something changed" just means "scan earlier than
  the next scheduled tick".

#### 5.9.2 Debouncing

Bursty events get collapsed:

- Watcher events for a source land in a per-source buffer.
- A 500 ms quiet window after the last event triggers the scan-tick
  request.
- If the source is already scanning/uploading, the request is
  remembered; another scan kicks off as soon as the current cycle
  ends.
- Hard cap: at most one scan-tick request per minute per source from
  the watcher (the scheduled scan still runs independently of this
  cap).

#### 5.9.3 Exclusion-aware filtering

The watcher does NOT filter by ignore/exclude patterns at the event
level — that's the scanner's job. We DO filter at the path-prefix
level (don't watch `.git/objects/` if the source's
`respect_gitignore` matches it, to avoid filling the event buffer
with git noise on dev folders). The `ignore` crate's gitignore
matcher is queried per-event-prefix; non-matching paths get
dropped.

#### 5.9.4 Watcher failures and degradation

- **Linux inotify watch limit exhausted** (`fs.inotify.max_user_watches`,
  default 8192 on most distros): `notify` returns an error per
  `RecommendedWatcher::watch` for the directory that exceeded the
  budget. Surface to user via activity log + a one-time tray
  notification with a copy-paste fix
  (`sudo sysctl fs.inotify.max_user_watches=1048576`). The source
  still gets backed up via the scheduled scan — not a hard failure.
- **macOS FSEvents coalescing / latency:** FSEvents can coalesce
  events across long quiet windows or skip a batch under load. The
  scheduled scan catches anything FSEvents misses; we document the
  expected p99 latency-to-detect as `max(scheduled scan interval, ~30s)`
  under coalescing.
- **Windows directory-handle invalidation** (volume unmount,
  permission change): `notify` surfaces an error; we re-watch on the
  next scheduled scan. Same fallback path.
- **Watcher thread death / crash:** the orchestrator notices the
  source's event channel is closed, logs `WatcherDied`, falls back
  to scheduled-only for that source, attempts to restart on the next
  orchestrator tick.

#### 5.9.5 Why not pure event-driven

Tempting (no scheduled scan, react only to events) but the failure
modes above all silently turn "saved a file" into "file never
backed up". Backup software's lowest-acceptable failure mode is
"slower than expected", not "silently lost". Scheduled scan is the
guard rail; the watcher is the latency win.

#### 5.9.6 What V1 does NOT do

- Watch ACL / metadata changes (we don't care about those).
- Watch outside the configured source roots (no global "watch /").
- React per-event with per-file uploads (only triggers a scan-tick).

### 5.10 Sleep / wake handling

A laptop sleeping or hibernating during an active sync invalidates a lot
of in-flight state: OAuth access tokens expire, network conditions
change, Drive's resumable upload sessions may be untouchable (Drive's
1-week session lifetime is wall-clock, not awake-clock), and the
filesystem watcher's event backlog may be lossy depending on platform.
Driven must detect resume from sleep and reset the relevant state
*before* trying to push work through stale connections.

#### 5.10.1 Detection per platform

- **Windows:** subscribe to `WM_POWERBROADCAST` messages.
  `PBT_APMSUSPEND` = entering sleep / hibernate.
  `PBT_APMRESUMEAUTOMATIC` and `PBT_APMRESUMESUSPEND` = resume.
  Reached via the `windows` crate's `RegisterPowerSettingNotification` +
  message-pump on a dedicated hidden window owned by the tray thread.
- **macOS:** IOKit `IORegisterForSystemPower` on a dedicated `CFRunLoop`
  thread, delivering `kIOMessageSystemWillSleep` / `kIOMessageSystemHasPoweredOn`
  (and acking `kIOMessageCanSystemSleep` so the system never waits on us).
  Chosen over `NSWorkspace` notifications: it is the documented C sleep/wake
  API, matches both the existing IOKit power reader and the Windows callback
  shape, and needs no AppKit main-thread run loop - so it is self-contained.
- **Linux:** systemd-logind via DBus.
  `org.freedesktop.login1.Manager` signal `PrepareForSleep(bool start)`
  (`true` = about to sleep, `false` = just woke). Reached via the
  `zbus` crate's `#[proxy]`-generated signal stream.

A `SleepWakeEvent` enum (`Suspending`, `Resumed`) is emitted by the per-OS
`driven-power` backend and mapped to the orchestrator's `PowerEvent` at the
run-loop boundary, flowing into the event loop alongside power-source and
network events (issue #33).

#### 5.10.2 On `Suspending`

- Pause every active orchestrator cycle gracefully (don't kill in-flight
  HTTP requests — they may complete before sleep actually starts).
- Snapshot the `last_active_at` timestamp for each in-flight resumable
  upload session into `pending_ops.payload_json` so we can decide
  whether to reuse the session on wake.
- Tray icon → yellow with "Suspending..." tooltip.

#### 5.10.3 On `Resumed`

In strict order:

1. **Defer 30 s.** Real-world wake events fire before network and
   keychain services are fully ready. Pushing immediately produces a
   burst of confusing failures.
2. **Re-probe the network** (§5.8 three-probe topology). Don't proceed
   until at least the OS-connectivity probe is green.
3. **Pre-emptively refresh every account's access token** rather than
   waiting for the next request to hit `expires_at`. A request issued
   over an expired access token wastes a round-trip and a token-endpoint
   call.
4. **Discard any resumable upload session whose pre-sleep `last_active_at`
   is older than 6 days OR whose total sleep duration would push the
   session past its 7-day lifetime.** Re-create from byte 0 (the
   reconciliation pass §5.6 handles the orphan ciphertext on Drive).
5. **Tell the filesystem watcher to re-scan from scratch.** On macOS in
   particular, FSEvents can lose events across sleep; pure-event-driven
   logic without this would silently miss anything edited mid-sleep.
   (The scheduled scan is the fallback either way — see §5.9.4.)
6. Resume normal orchestrator ticks.

#### 5.10.4 Sleep mid-upload

Worst case: laptop sleeps mid-PUT-of-a-resumable-chunk. The TCP
connection gets a RST or hangs; the per-request timeout (§5.8.4)
fires; the chunk retries; the retry hits a stale session URL; Drive
returns 4xx; the executor follows the §5.4 session-restart path
(create fresh session, start from byte 0). All recoverable; the
auto-discard logic in step 4 above just makes the recovery proactive
rather than reactive.

---

## 6. Authentication

### 6.1 BYO OAuth client wizard

> **Critical:** the consent screen MUST be moved from "Testing" to "In
> production" or the user will be logged out every 7 days — exactly the
> pain point Driven exists to fix. Google's policy is unambiguous:
> *"A Google Cloud Platform project with an OAuth consent screen
> configured for an external user type and a publishing status of
> 'Testing' is issued a refresh token expiring in 7 days, unless the
> only OAuth scopes requested are a subset of name, email address, and
> user profile"*
> (https://developers.google.com/identity/protocols/oauth2). Publishing
> the consent screen to "In production" lifts the 7-day cap and works
> for **unverified** self-owned apps requesting sensitive scopes — the
> user just sees a one-time "Google hasn't verified this app" click-through
> the first time they sign in, then the refresh token persists
> indefinitely (subject to the standard 6-month-of-disuse rule).

First-run flow (~5 min of clicking through GCP):

1. **Welcome panel** explains *why* BYO credentials (long-lived refresh
   tokens, your own quota budget, no Google verification needed for
   single-user use). Sets expectations: there will be a one-time
   "unverified app" warning to click through on sign-in; that warning is
   inherent to using full Drive scope on an unverified self-owned app
   and cannot be removed without paying for Google's audit.
2. **Step 1 — GCP project:** open
   `https://console.cloud.google.com/projectcreate`. User creates a
   project (any name).
3. **Step 2 — Enable Drive API:** open
   `https://console.cloud.google.com/apis/library/drive.googleapis.com`
   for the new project; user clicks "Enable".
4. **Step 3 — Configure OAuth consent screen:** open
   `https://console.cloud.google.com/apis/credentials/consent`. Pick
   "External" user type. Fill out app name, support email, developer
   contact (all the user's own email). Add the `drive` scope when
   prompted. **Do not** add test users — we are not staying in Testing.
5. **Step 4 — Publish the consent screen to production:** in the OAuth
   consent screen sidebar click **"Publish app"**. Confirm the
   "Your app will be available to any user with a Google Account"
   dialog. The verification status will read "needs verification" for
   the sensitive scope; that is fine and expected for a single-user app.
   **This step is the one that fixes the 7-day relogin pain.**
6. **Step 5 — Create the OAuth client:** open
   `https://console.cloud.google.com/apis/credentials/oauthclient`. Pick
   **"Desktop application"** type. Name it (e.g. "Driven"). Hit Create.
   Wizard surfaces a copy-paste help panel for the resulting Client ID
   and Client Secret.
7. **Step 6 — Paste credentials:** user pastes Client ID + Client
   Secret into the wizard. App stores them in the OS keychain via the
   `keyring` crate, keyed by a Driven-generated account UUID
   (deliberately NOT keyed by the Google account email — the user
   hasn't authenticated yet at this step).
8. **Step 7 — Sign in:** wizard kicks off the standard
   authorization-code-with-PKCE flow against `accounts.google.com`.
   Redirect URI is `http://127.0.0.1:<random-free-port>/oauth/callback`.
   The app spawns a short-lived `axum` server on that port, opens the
   browser, the user clicks through the "Google hasn't verified this
   app" warning (`Advanced` → `Go to <name> (unsafe)`), then sees the
   normal Google consent screen, then is redirected back to the
   loopback. We capture the code, exchange for tokens, store the
   refresh token in the keychain, fetch the user's profile to populate
   the account row's email + display name.

For **subsequent accounts**: skip steps 1–5 (reuse the same GCP project
+ OAuth client). Step 6 reuses the stored Client ID/Secret. Step 7
re-runs against the new Google account.

Wizard UX detail: every "open this URL in the browser" step is
accompanied by a "Done — continue" button and an "I'm stuck" link that
opens a short troubleshooting panel.

#### What we lose by staying unverified

- The one-time "Google hasn't verified this app" click-through per
  Google account. Unavoidable for sensitive scopes without Google's
  paid verification process.
- Two limits to keep in mind (these are *separate* — easy to conflate):
  - **Unverified-app user cap**: Google caps an unverified
    sensitive-scope app at **100 distinct end users** giving consent.
    Per-user. Resets on Google's audit completion. For a personal
    BYO-credentials tool with one human owner this is a non-issue.
  - **Refresh-token-per-(client,account) cap**: Google enforces a
    limit of **100 refresh tokens per (OAuth client, Google account)
    pair**. Issuing a 101st refresh token silently invalidates the
    oldest. Driven issues one refresh token per (account, machine), so
    this only matters if the same Google account is signed in on 100+
    Driven installs. Also a non-issue in practice.

#### What Internal-Workspace users get (carefully)

If — and only if — the user is on a Google Workspace org **AND** they
own (or have admin access to) a Cloud Identity / Workspace-tied GCP
project, they can set the OAuth consent screen to "Internal" instead
of "External". Internal apps skip the unverified-app warning and stay
on long-lived tokens with no "Publish" step. Caveats: Internal apps
require a project owned by the Workspace organisation (not a personal
GCP project), and may require Workspace admin approval to install
based on org policy. Driven's wizard surfaces this as an **advanced
optional path** for confirmed-Workspace users with project-admin rights
— not the default flow, because most personal users will not satisfy
the prerequisites.

### 6.2 Token storage

| Item                | Location                                             |
|---------------------|------------------------------------------------------|
| OAuth client_id     | OS keychain (`driven/account-{id}/client-id`)        |
| OAuth client_secret | OS keychain (`driven/account-{id}/client-secret`)    |
| Refresh token       | OS keychain (`driven/account-{id}/refresh-token`)    |
| Access token        | In-memory only (refreshed on demand)                 |
| Encryption key      | OS keychain (`driven/account-{id}/master-key`)       |

`keyring` crate is the canonical cross-platform wrapper. The SQLite DB
contains only opaque account IDs + non-secret metadata; the file is safe
to leave on disk unencrypted.

### 6.3 Refresh

A thin `RefreshingTokenSource` (see SPEC §4.1) wraps the access token +
its expiry and the keychain-stored refresh token. On every Drive
request, if `expires_at - now < 60s` it POSTs to
`https://oauth2.googleapis.com/token` with `grant_type=refresh_token`
and persists the new access token in memory. We do **not** use
`yup-oauth2` — the OAuth flow and refresh wrapper together are a few
hundred lines and we want one code path, not two.

When refresh fails with `invalid_grant`, the orchestrator marks the
account `needs_reauth`, fires an OS notification, and surfaces a banner in
the Settings window with a "Re-link" button that re-runs step 7 of the
wizard (the sign-in step, not the GCP click-through).

### 6.4 What is and isn't a secret

For OAuth installed-app clients ("Desktop application" type), Google
explicitly classifies the **client_secret as a public credential** —
see https://datatracker.ietf.org/doc/html/rfc8252 §8.5: "the client
secret should not be considered confidential". The real protection on
the OAuth flow is PKCE + the loopback redirect URI, not the secret.
Driven treats client_secret accordingly: we store it in the OS keychain
(less casual disk-dump exposure, no special handling beyond that), we
send it on token exchange + refresh because Google's token endpoint
still requires it for installed-app clients, but we do not pretend it's
high-value. The high-value secret on the desktop is the **refresh
token**: keychain-only, never logged, never serialised to disk
plaintext, never sent in telemetry.

---

## 7. Encryption

V1 ships **client-side encryption** as a per-source toggle. Both
content and filename encryption use **XChaCha20-Poly1305** (extended
nonce, 192-bit) — content via the audited STREAM construction
(Hohenberger-Lewko-Waters), filename via a single AEAD call with a
deterministic nonce derived from the path.

Why XChaCha20-Poly1305 specifically:
- 192-bit nonce removes any practical concern about nonce collisions
  from deterministic derivation (BLAKE3 collisions at 192 bits are
  computationally infeasible: 2^-96 per pair).
- STREAM construction over it gives chunked AEAD with authenticated
  reordering / truncation resistance — exactly what we need for
  large-file streaming uploads.
- Single Rust crate (`chacha20poly1305`) ships both primitives. No
  `age` wrapper, no X25519-recipient indirection: we have a 32-byte
  key directly and the public `aead::stream` module is built for this.

Why **not** rclone-crypt-compatible by default:
- rclone-crypt's filename scheme uses EME-AES with a non-trivial
  base32-without-padding encoding that has caused subtle interop bugs.
- The chacha20poly1305 STREAM construction is a cleaner audited
  primitive; the marginal benefit of rclone-decryptability is small
  for the target user.

If users ask for rclone-recoverability we'll add an opt-in second
format in V2 (already in §17 deferred list).

The format is documented in `design/CRYPTO_FORMAT.md` so a Rust CLI
tool (`driven-cli decrypt <path>`) can decrypt without the GUI app.

### 7.1 Format

- **Master key:** 32 random bytes per account, stored in OS keychain.
- **Per-source key:** 32 random bytes, wrapped (encrypted) with the
  master key via XChaCha20-Poly1305 + random 24-byte nonce stored
  alongside the ciphertext in the `backup_sources.wrapped_source_key`
  column. Lets us rotate per-source keys without re-uploading other
  sources, and revoke a specific source's data by destroying its key.
- **File content encryption:** `chacha20poly1305::aead::stream::EncryptorBE32<XChaCha20Poly1305>`:
  - 64 KiB plaintext chunks → 64 KiB + 16-byte tag ciphertext chunks.
  - **24-byte nonce per file**, randomly generated and written into a
    16-byte-magic + 24-byte-nonce file header (40 bytes).
  - 32-bit big-endian chunk counter automatically incremented + last-chunk
    flag set on the final chunk per the STREAM construction.
  - Per-source key is the AEAD key (file-level nonce header gives
    per-file domain separation).
  - Provides authenticity + prevents chunk reorder/truncate/duplicate
    attacks across the entire file.
- **Filename encryption:** **two derived sub-keys** from the per-source
  key via BLAKE3's keyed-derive-key (`derive_key`) with distinct
  domain-separation contexts (avoids using the same key bytes in two
  roles):
  - `nonce_key = BLAKE3::derive_key("driven-filename-nonce-v1", per_source_key)`
  - `aead_key  = BLAKE3::derive_key("driven-filename-aead-v1",  per_source_key)`
- Each path component is encrypted independently with
  XChaCha20-Poly1305 (the 192-bit-nonce variant) using:
  - **Plaintext canonicalisation** before nonce derivation:
    NFC-normalise the full path-from-source-root, then case-fold per
    the source's filesystem-case-sensitivity setting, then UTF-8 encode
    to a canonical byte sequence. Without this step, two paths that
    look equal to the user (`café` NFC vs `café` NFD) would encrypt to
    different ciphertexts and the restore browser would show
    duplicates.
  - **Nonce:** 24 bytes from `BLAKE3::keyed_hash(nonce_key, canonical_path_bytes)`
    truncated to 24 bytes. Deterministic so the same canonical
    plaintext always maps to the same ciphertext — needed for
    `files.list` lookups to work. Domain-separated and key-separated
    from content encryption. The 192-bit nonce space is wide enough
    that deterministic derivation is safe (collision probability
    2^-96 per pair).
  - **AEAD:** `XChaCha20Poly1305::encrypt(nonce, plaintext, aad)` with
    `aead_key`, AAD = the parent folder's ciphertext name (binds a
    child to its parent — moving a folder forces all children to
    re-encrypt under the new AAD).
  - **Encoding:** base32hex (RFC 4648 §7, lowercase, no padding) so
    the ciphertext is valid as a Drive filename across all platforms.
- **What this leaks** (documented, not hidden):
  - **Equality across snapshots within a source:** an adversary with
    Drive read-access can tell "this file's name has not changed".
    Acceptable for a backup tool.
  - **Cardinality:** the number of files per folder.
  - **Filename length, modulo a small constant:** XChaCha20-Poly1305
    ciphertext length = plaintext length + 16 (the tag); base32hex
    encoding then expands by 8/5. So an N-byte plaintext component
    becomes `ceil((N + 16) * 8/5)` ciphertext bytes. **V1 ships this
    leak**; padding to fixed-size buckets (e.g. 16-byte buckets up to
    256, then 64-byte buckets) is a V2 option but doubles average
    name length — not worth it for the typical threat model.
  - **File size, modulo chunk count + header:** chunked-AEAD content
    leaks the number of 64-KiB chunks plus a 40-byte header. Hiding
    this requires file-level padding which is wasteful for backup.
    Not addressed in V1.
- **No cross-source correlation:** each source's `per_source_key` is
  independently random, so filename ciphertext is uncorrelatable
  across sources or accounts.
- **Hash columns:** local state stores **plaintext** blake3 (for change
  detection); Drive's `md5Checksum` is the **ciphertext** md5 (because
  that's what Drive computes). We never need to round-trip the
  plaintext through Drive to detect change.

### 7.2 What we explicitly DON'T do

- Roll our own chunked-AEAD construction. STREAM (Hohenberger et al.)
  is the reference design; the `chacha20poly1305::aead::stream` module
  is the audited Rust implementation.
- Use a deterministic nonce on chunk content — chunks within a file
  use STREAM's monotonic counter, not a hash of the chunk plaintext.
  (Deterministic nonces on content would enable equality detection
  across files.) Deterministic nonces are confined to filename
  encryption, where the wide 192-bit nonce + the equality-detection
  requirement together make it safe and necessary.
- Re-encrypt all files on per-source key rotation. We keep the old
  per-source key as a "decrypt-only" key in a `legacy_source_keys` table
  so already-uploaded files stay readable; new uploads use the new key.
  Optional `driven-cli rekey` re-uploads all files under the new key.

### 7.3 Restore from encrypted backup

The Restore browser decrypts paths on read, so the user sees plaintext
filenames in the UI. Restoring a file fetches the ciphertext, streams it
through chunked decryption, writes to the chosen local path.

If the master key is lost (keychain wiped, machine reformatted), the user
can paste a previously-exported recovery phrase (BIP39 over the master
key) into the setup wizard. The recovery phrase is shown on encryption
opt-in and the user must check "I've stored it somewhere safe" to proceed.

---

## 8. UI

### 8.1 Tray

Always-present icon. States:

| Icon                  | Meaning                                                                     |
|-----------------------|-----------------------------------------------------------------------------|
| Default (gray)        | Idle, last sync OK                                                          |
| Animated (spinner)    | Sync in progress                                                            |
| Yellow                | Paused (user or auto: battery, metered, schedule)                           |
| Yellow with `!` badge | Network attention: offline / captive portal / Drive unreachable             |
| Red                   | Error state requires attention (auth needed, decrypt failure, disk full)    |

The yellow-with-`!` state covers all of §5.8's network failure modes.
The tooltip shows the specific condition ("Connected, no Internet",
"Captive portal — click to sign in", "Google Drive is unavailable", etc.)
and the tray menu surfaces the matching action ("Open captive portal",
"Retry now", "Open Google status page").

Tray menu:
- "Driven — Last sync: 3m ago"
- ─────
- ▶ "Sync now"
- ⏸ "Pause for…" → 30m / 1h / 4h / Until manual resume
- "Settings…"
- "Activity…"
- "Restore…"
- ─────
- "About / Updates…"
- "Quit"

Left-click on the icon opens Activity by default (configurable). On
Linux this may not fire — per Tauri v2 docs, tray icon click/mouse
events are documented as unsupported on Linux desktop environments —
so the **menu is the canonical interaction surface** across platforms.
Everything reachable via left-click is also a menu item (or surfaces
the same window via a menu item) so users on Linux are never stuck.

### 8.2 Settings window

Tabs:
- **Accounts** — connected Google accounts, add/remove, re-auth status.
- **Backup sources** — table of sources: enabled toggle, local path, Drive
  destination, account, encryption on/off, "Edit exclusions" button,
  schedule, "Run now", "Remove".
- **Add source wizard** — local folder picker → Drive folder picker
  (uses the chosen account) → exclusion preview → encrypt? → confirm.
- **Rules** — global rules: battery, metered, bandwidth cap, concurrent
  uploads, deep-verify interval. (Schedule windows are V2 — see §17.)
- **About** — version, update channel, "Check for updates", license,
  release notes viewer.

### 8.3 Activity dashboard

- Header: aggregate stats — bytes uploaded today / week, file count
  by status, current throughput.
- Live tail (last 1000 events) from `activity_log`, filterable by
  source / level / type.
- Persisted to SQLite so it survives restart (last 30 days minimum).
- "Export diagnostic bundle" button: zips logs + sanitized config +
  schema-version + recent activity, drops in user-chosen folder.

### 8.4 Restore browser

- Tree view of backed-up paths, scoped by selected source.
- Decrypts filenames inline if source is encrypted.
- Toolbar: search box (filename / glob), "Restore selected" button,
  "Restore to…" target picker.
- Backed by SQLite FTS5 over `file_state.relative_path` for instant search.
- Streams files from Drive on demand — does not pre-fetch.

### 8.5 Setup wizard

Triggered on first run and from "Add account". Multi-step modal in the
Settings window:

1. Welcome / what is Driven
2. BYO-credentials walkthrough (§6.1)
3. Pick first backup source
4. Encryption opt-in (with recovery-phrase reveal)
5. Initial sync confirmation

### 8.6 Frontend tech

Vue 3 + Vite + TypeScript + Pinia + Tailwind CSS + **vue-i18n** (see §8.7).
Vue per the user's preference in their global CLAUDE.md. Pinia for
cross-component state. Tailwind for styling consistency without a heavy
component library.

We do **not** use Nuxt - Nuxt's SSR / server routes don't fit a Tauri
shell. We don't use a UI framework like Vuetify/PrimeVue either; the UI
surface is small enough that hand-rolled components on Tailwind keep
bundle weight low (a Tauri app's webview is full Chromium-equivalent
weight; bundle size still matters for first paint).

### 8.7 Internationalization (V1 prep, V2+ translations)

**V1 ships English (`en-US`) only but is i18n-ready from day one.**
Adding a language is a pure-translation contribution (new locale file),
never a code change. Every user-facing string in the codebase must flow
through a translation function from M0 onward - never a hardcoded
literal.

#### Frontend (the bulk of user-facing strings)

- **`vue-i18n` v11.x** with the `@intlify/unplugin-vue-i18n` v11.x Vite
  plugin for compile-time message extraction (v9 is EOL; v11 is current
  as of mid-2026 and supports Vite 5/6).
- Locale files live under `ui/src/locales/<lang-tag>.json`. V1 ships
  `en-US.json`; CONTRIBUTING.md documents how to add a new locale.
- Translation keys are dotted-namespace strings:
  `settings.account.removeButton`, `wizard.oauth.step5.title`,
  `errors.drive.rateLimited.short`, `errors.drive.rateLimited.long`.
- **ICU MessageFormat** for plurals + interpolation:
  `{count, plural, one {# file uploaded} other {# files uploaded}}`.
  Handles language-specific plural rules from V1.
- Lint rule `@intlify/vue-i18n/no-raw-text` fails CI on any Vue template
  containing a raw English literal in user-visible position.
- Date / number / byte-count formatting via `Intl.DateTimeFormat` /
  `Intl.NumberFormat` driven by the active locale - never via custom
  English formatters.

#### Backend (small surface)

The backend's user-facing string surface is small: tray menu labels,
OS notification titles + bodies, autostart launcher display name.
Everything else (error messages, activity log entries) the backend
emits as a **stable error code** (SPEC §24) + structured payload; the
frontend looks up `t('errors.${code}.short')` / `${code}.long`. The
backend never emits user-facing English error messages.

For the small string surface that DOES live in Rust:
- **`rust-i18n` crate** with locale files under `src-tauri/locales/<lang>.yml`.
- Loaded at startup; emits tray menu in the active locale; re-emits on
  locale-change event from the frontend.

#### Locale selection

- App reads the OS locale via the `sys-locale` crate
  (`sys_locale::get_locale()`) on first run, picks the closest
  available match (falls back to `en-US`), persists choice in the
  `settings` KV table under `ui.locale`.
- User can override in Settings → About → "Display language" (only shows
  a selector once V2 actually has > 1 locale; V1 surfaces a "more
  languages coming" placeholder).
- Locale changes apply live without restart - vue-i18n is reactive;
  rust-i18n re-renders tray menu on a `locale_changed` event.

#### Stable error codes are load-bearing for i18n

Every code in SPEC §24's error taxonomy is the canonical key for the
translation lookup. Therefore:
- Codes MUST NOT change between minor versions (would orphan
  translations).
- New codes are purely additive.
- Deprecated codes stay translatable for at least one major release.

#### What V1 does NOT do (V2)

- Right-to-left layout (Arabic, Hebrew). Tailwind has `rtl:` variants;
  the actual UI flip is V2 work.
- Translation of release-notes / changelog content - those come from
  maintainer commit messages and stay in English.
- Dynamic language fallback chains (e.g. `fr-CA` → `fr-FR` → `en-US`);
  V1 does direct match-or-default-to-en-US.

---

## 9. Update mechanism

### 9.1 Channels

| Channel | Built from               | Visible to user             |
|---------|--------------------------|-----------------------------|
| stable  | Tagged release (`v*`)    | Always                      |
| dev     | Per-commit on `main`     | Opt-in via Settings → About |

Tauri's updater plugin reads a different `update.json` URL depending on
the configured channel.

### 9.2 Signed updates

Tauri v2's built-in updater uses an ed25519 keypair (the `tauri signer`
CLI). The public key is baked into `tauri.conf.json`; the private key is a
GitHub Actions secret. Every build is signed; the client verifies before
applying.

### 9.3 In-app changelog viewer

On update available, the in-app banner has "What's new" → opens a modal
that renders the GitHub release body for the new version. We also keep a
"What's new in this version" entry permanently in Settings → About → "View
recent releases", which paginates the GitHub releases API and renders the
markdown bodies.

Release notes themselves are generated automatically by
**[release-please](https://github.com/googleapis/release-please)** from
[Conventional Commits](https://www.conventionalcommits.org/) on the
`main` branch. release-please opens a release PR that, when merged, tags
the release and the GH Actions build pipeline takes over.

### 9.4 Update cadence

- App polls the update endpoint on startup, then every 6 hours.
- New stable → banner asks the user to apply. Default deferral: 24h
  reminder.
- New dev → applies silently with a tray notification (dev channel is for
  power users who want the freshest build).

---

## 10. Reliability & data integrity

- **SQLite in WAL mode.** Concurrent reader during ongoing writer.
  Periodic `PRAGMA wal_checkpoint(TRUNCATE)` to keep WAL bounded.
- **Per-op transactions.** Each `pending_ops` row processed inside one
  transaction with its target table update. Crash mid-op leaves the row
  in `pending` and we replay on next start.
- **Resumable upload URLs** live up to one week per
  https://developers.google.com/drive/api/guides/manage-uploads#resumable.
  We persist them with their issued-at timestamp and discard if older
  than 6 days before resuming.
- **md5 verification on upload** — see §5.4.
- **Periodic deep-verify** — see §5.2. Catches:
  - Local bit-rot via mismatched plaintext hash.
  - Drive-side corruption via mismatched md5.
  - Filesystems lying about mtime (rare but real on some network mounts).
- **Backup of state DB.** Driven itself writes the SQLite state DB to a
  **dedicated** Drive folder (`<Driven root>/_driven-meta/state-db/`) on
  the user's primary account every N hours so the user can restore even
  if the local DB is destroyed. **The state DB contains plaintext paths,
  encrypted-source plaintext filenames in the FTS index, and metadata
  that an attacker with Drive read-access could analyse — so the backup
  is itself age-encrypted with the account master key before upload.**
  Recovery requires the master key (in the OS keychain, or recoverable
  from the BIP39 phrase from setup). Without the master key, the backup
  is opaque ciphertext. This folder is **never scanned as a local
  source** — uploading the state DB into a watched source folder would
  cause it to be re-uploaded immediately and recursively on every
  cycle. The scanner explicitly skips `_driven-meta/` and the local
  meta path (`<config_dir>/driven/state.db*`) is never on a
  backup-source allowlist.

---

## 11. Performance

### 11.1 Targets
- Idle CPU **≤ 1%** averaged over a minute.
- Idle RAM **≤ 100 MB** RSS for the backend, plus webview only when an
  app window is open.
- Scan throughput **≥ 50k files/sec** on a warm cache for an unchanged
  source. (Equivalent to ripgrep walk speed minus the SQLite lookup
  cost.)
- Initial first-time backup throughput: **bottlenecked by the slowest
  of {network, disk, Drive's quota, CPU}** — never by single-threaded
  Driven code. On a 1 Gbit symmetrical link, expect to saturate the
  network for streams of medium+ files (>= ~1 MB) once the pacer has
  probed up to Drive's per-account ceiling. For tiny files the per-API
  round-trip dominates (this is the motivation for §17's tar.gz V2
  bundling).
- p50 per-file upload latency on a 1 Gbit link: < 200 ms for files
  ≤ 1 MB (one Drive round-trip dominates), throughput-bound thereafter.

### 11.2 Techniques
- **blake3** for hashing. Roughly an order of magnitude faster than
  sha256 on modern CPUs.
- **mmap big files** (> 4 MB) for hashing to avoid double-buffering.
  Fall back to streaming read if mmap fails (network mounts).
- **Lowered I/O priority** during scans:
  - Windows: `SetThreadInformation(... ThreadPowerThrottling ...)` +
    `SetPriorityClass(BELOW_NORMAL_PRIORITY_CLASS)`.
  - macOS: `setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD,
    IOPOL_THROTTLE)`.
  - Linux: `ioprio_set(IOPRIO_CLASS_IDLE)`.
- **Bounded channels** between scanner → planner → uploader so a fast
  scanner doesn't OOM the queue.
- **Streaming hash + upload.** No "hash first, upload second" two-pass.
  We pipe the file through a tee reader that feeds the hasher and the
  HTTP body simultaneously.
- **Pacer with jitter.** Avoids thundering-herd against Drive after a
  pause/resume.

### 11.3 Anti-AV friendliness
- Open files with `FILE_SHARE_DELETE` (§5.3) so we don't trigger
  antivirus "this process is locking my files" heuristics.
- Don't hammer the same directory in tight loops; the AV scanner is
  context-sensitive to access patterns.

### 11.4 Concurrency model

Driven exploits two distinct parallelism axes. The design exists
because Drive's API supports only one of them — knowing which is
where the speed actually comes from.

#### 11.4.1 What Drive lets us parallelise

| Axis                                | Drive supports? | Why / why not                                                                                                              |
|-------------------------------------|-----------------|----------------------------------------------------------------------------------------------------------------------------|
| **Multiple files at once**          | **Yes**         | Each upload is an independent HTTPS request. HTTP/2 multiplexes them over a single TCP connection. Bounded by the per-account QPS pacer (§5.4). |
| **Multiple chunks of one file**     | **No**          | A resumable upload session accepts chunks **in order, with monotonically increasing offsets**. There is no documented or undocumented way to issue parallel chunks against one session — confirmed via Drive docs (https://developers.google.com/workspace/drive/api/guides/manage-uploads#resumable) and the open feature requests on Google's issue tracker (issue #220523936 / #132489107, both unresolved as of the design review). |
| **Multiple uploads of one shard-split file** | Possible workaround | We could split a 50GB file locally into N "shard" Drive objects, upload in parallel, store a manifest. **Explicit non-goal** for V1 (DESIGN §2: "Each file maps to one Drive object") — breaks Drive's browsability and locks users into Driven-format restore. |

So all per-file speedup must come from **pipelining the local-side
work concurrently with the upload**, not from sharding the upload
itself.

#### 11.4.2 Inter-file parallelism (the big win)

A per-account `UploadPool` with **`min(num_cpus * 2, 16)` permits by
default** (configurable down to 1 for diagnostics, up to a hard cap
of 32). Each permit serves one in-flight file. Rationale:

- 8-16 in-flight requests against Drive's HTTP/2 single-connection
  pool typically saturates a 1 Gbps link for medium files.
- The factor of `2 * num_cpus` (vs `1 * num_cpus`) is because each
  file is mostly IO-bound during the upload phase — CPU is only busy
  during the hash+encrypt slice of its lifetime.
- The hard cap of 32 stops the pool from going crazy on a 64-core
  workstation and starving Drive's per-account quota.
- The pacer (§5.4) is the canonical rate limit; the pool size is just
  "how much work can be in-flight". Per-second budget is the pacer's.

Cross-account: unbounded. If you have 3 Google accounts each backing
up a different folder, all 3 run their own `UploadPool` in parallel.

#### 11.4.3 Intra-file pipelining (the small but free win)

For each in-flight file (any size), the work splits into a 3-stage
pipeline; bounded mpsc channels between stages provide backpressure:

```
   [Reader task]      [CPU task]              [Uploader task]
       |                   |                       |
   read 1MB chunk ─chan→  hash + encrypt  ─chan→  PUT chunk N
   read 1MB chunk ─chan→  hash + encrypt  ─chan→  PUT chunk N+1   (queued; waits for N to ack)
   read 1MB chunk ─chan→  hash + encrypt  ─chan→  PUT chunk N+2   (queued)
       |                   |                       |
   bounded(4)          bounded(4)              sequential (Drive constraint)
```

What this buys: while chunk N is in flight to Drive (which dominates
wall-clock), the reader is grabbing chunk N+2 from disk, and the CPU
task is encrypting chunk N+1. None of the stages block each other
until the channel fills. The result is that one file's effective
throughput approaches `min(disk read, hash+encrypt, network upload)`
rather than their sum.

- **Reader task** runs on a tokio task — `tokio::fs::File::read`.
- **CPU task** runs on a `rayon` worker — hashing + encryption are
  CPU-bound, blocking-style, and `tokio::task::spawn_blocking` would
  starve the tokio reactor under load.
- **Uploader task** runs on a tokio task — `reqwest::put` with a
  `Body::wrap_stream` over the channel of encrypted chunks.

The chunk size flowing through the pipeline (the size of one queue
entry) is **1 MiB** by default. This is independent of the on-disk
file read buffer (`64 KiB`) and the HTTP wire chunk (sized to be a
multiple of 256 KiB per Drive's spec, default 4 MiB — accumulated
from 4 pipeline chunks before being sent).

#### 11.4.4 Multi-core hashing for big files

`blake3`'s `Hasher::update_rayon()` parallelises the hash of a single
contiguous buffer across `rayon`'s threadpool — useful when one file
is large enough that single-threaded hashing is the bottleneck
(roughly: files > 100 MB on a typical 8+ core CPU, where single-core
blake3 is ~1 GB/s and disk is faster).

Strategy:
- **Files ≤ 4 MiB**: hash inline in the read path (one task), no
  pipeline, no rayon. Overhead would dominate.
- **Files 4 MiB – 100 MiB**: full 3-stage pipeline, single-threaded
  hash per chunk inside the CPU task.
- **Files > 100 MiB**: full 3-stage pipeline, AND the CPU task uses
  `blake3::Hasher::update_rayon` per chunk (or fed via `mmap` if the
  filesystem supports it — see §11.2). For files > 1 GiB,
  `update_rayon` over the full mmap'd file can hash at 5+ GB/s on
  modern desktops.

The MD5 used for Drive's `md5Checksum` verification stays
single-threaded — MD5 doesn't parallelise across input and is fast
enough on one core to keep up with any realistic upload rate.

#### 11.4.5 Tokio + rayon split

| Threadpool        | Sized          | Used for                                                                |
|-------------------|----------------|-------------------------------------------------------------------------|
| `tokio` reactor   | `num_cpus`     | All async IO: HTTP, file IO, SQLite via tokio-tied executor, channels.  |
| `rayon` global    | `max(num_cpus - 1, 2)` | All CPU work: hashing, encryption, deep-verify hashing. One core reserved for tokio so the reactor never starves. |

Anything explicitly CPU-bound goes through `rayon::spawn` or
`spawn_blocking`-into-rayon. `tokio::spawn` is for async tasks only.
Mixing the two reliably (CPU on rayon, async on tokio) is what keeps
GUI events + tray updates + status emits responsive while a big
backup is hammering the disk and the network.

#### 11.4.6 Memory bounds under maximum parallelism

Worst case at the default settings:
- 16 files in flight × 4 MiB HTTP wire buffer × 2 (one being uploaded,
  one being prepared) = **128 MiB** for the upload buffers.
- 16 × 4 pipeline channel entries × 1 MiB chunk = **64 MiB** for
  in-flight pipelined data.
- Hash state, rayon scratch, miscellaneous: ~50 MiB.

Total upper bound for the backend during heavy backup: **~250 MiB
RSS**. Well below any consumer machine's limits, well above the
≤100 MiB idle target.

If the user is on a constrained machine (Settings → Rules →
"Conservative resources" mode), the worker pool drops to
`num_cpus / 2` and the wire-chunk size halves — caps RSS around
80 MiB during heavy backup.

#### 11.4.7 Adaptive parallelism

A `ThroughputProbe` watches the per-account upload throughput over
30-second windows. If sustained throughput is < 50% of the previous
window's, AND the pacer is NOT throttling, AND the local disk is
not saturated (`iostat`-equivalent), it **reduces the in-flight pool
size by 1**. If sustained throughput is at the pool's ceiling AND
the disk + CPU have headroom, it **raises the pool size by 1** (up
to the hard cap).

This protects against the pathological case where you've got 16
files in flight but Drive's edge is overloaded and each takes 2x
longer than running 8 in flight would — fewer-in-flight = each one
finishes faster = higher net throughput.

#### 11.4.8 Scanner & planner parallelism

- The walker itself is **single-threaded** (the `ignore` crate). The
  cost is tiny compared to hashing/uploading for any non-empty change
  set, and parallel `ignore` walking has correctness pitfalls around
  the `.gitignore` cascade that aren't worth re-validating.
- Hashing during a **deep-verify pass** (re-hash everything to catch
  bitrot) **does** use rayon — it's pure CPU+disk-read, no Drive
  involvement. Defaults to `num_cpus - 1` workers.
- The planner is single-threaded; its work is trivial compared to
  any other stage.

#### 11.4.9 What we DON'T parallelise

- The single-resumable-session per-file constraint (Drive). Inter-
  file is the substitute.
- The orchestrator state machine — single-task per account so state
  transitions are linearisable. Per-account orchestrators run in
  parallel.
- SQLite writes — single writer (WAL mode allows concurrent readers).
  All `pending_ops` / `file_state` mutations go through one tokio
  task per account so they don't fight the lock.

---

## 12. Threat model (overview)

Detailed in `design/THREATS.md`. Bullet summary:

| Asset                  | Threat                            | Mitigation                                  |
|------------------------|-----------------------------------|---------------------------------------------|
| OAuth refresh token    | Disk theft / malware              | OS keychain only                            |
| Encryption master key  | Disk theft / malware              | OS keychain only; never written to disk     |
| File contents on Drive | Google-side compromise            | Optional client-side encryption (§7)        |
| Filenames on Drive     | Google-side metadata leak         | Optional filename encryption (§7)           |
| Update binary          | Malicious GH Releases asset       | Tauri ed25519 signature verification        |
| Local SQLite DB        | Disk theft                        | Contains no secrets; metadata only          |
| Telemetry              | Identifying user                  | Random install ID, no PII, opt-out          |
| Local IPC              | Untrusted webview content         | Tauri allow-list; no http/file:// content   |

---

## 13. Telemetry

**Opt-out, anonymous.** Default on so the maintainer (the user) sees real
usage signal. Trivial to disable in Settings → Privacy.

Payload (sent on startup, then daily):
- Anonymous install ID (random UUID v4, generated on first run, stored in
  config).
- App version, OS family + version, CPU arch.
- Counts since last ping: files uploaded, bytes uploaded, errors by
  category, deep-verify runs, update applied.
- Latency histograms (p50, p95) for scan and upload-per-file.

**Never sent:** file paths, file contents, account emails, Drive folder
IDs, Google client_id, error messages with stack traces beyond
category.

Backend: a tiny Cloudflare Worker writing to Analytics Engine — fits in
free tier and matches the user's stack preferences (Cloudflare > Railway
for frontend-y deploys per CLAUDE.md).

Crash reports are *separate*: a panic hook writes the backtrace to a
local file. The user clicks "Export diagnostic bundle" in Activity and
chooses where to put the zip. Nothing is auto-uploaded.

---

## 14. Testability (foundational)

This is treated as a first-class requirement, not an afterthought,
because the project specification calls out: *"everything must be
testable by yourself without any input from me, via mocked / unit test
and real backup, with fast debugging for as fast iteration as possible."*

### 14.1 Layers

| Layer                | Tool                                 | Runtime |
|----------------------|--------------------------------------|---------|
| Unit                 | `cargo test` per crate               | < 5s    |
| Integration (core)   | `cargo test -p driven-core`          | < 30s   |
| Integration (drive)  | `cargo test -p driven-drive`         | < 30s   |
| End-to-end (fake)    | `cargo test --test e2e_fake`         | < 60s   |
| End-to-end (real)    | `cargo test --test e2e_real` (gated) | < 5min  |
| UI component         | `pnpm test:unit` (Vitest)            | < 10s   |
| UI E2E (V1.1)        | `pnpm test:e2e` (Playwright on built Tauri binary) - DEFERRED to V1.1 | < 5min |

> **UI E2E (Playwright) is DEFERRED to V1.1** - there is intentionally no
> Playwright in the repo for V1. The first-run setup wizard, the flow the
> `pnpm test:e2e` row was meant to gate, is covered in V1 by a Vitest jsdom +
> Vue Test Utils mount test (`ui/src/__tests__/setup-wizard.test.ts`) that
> walks all five DESIGN §8.5 steps against the fake remote (the mocked IPC
> seam). That jsdom mount is the ACCEPTED V1 substitute; a browser-driven
> wizard smoke on the built bundle lands in V1.1. See the M6 acceptance
> criteria in ROADMAP.md and "Beyond V1 (V1.1+)".

### 14.2 The DriveClient seam

The canonical trait definition lives in **SPEC §3** (which carries the
full method set: `ensure_folder`, the `create` / `update` split, the
`appProperties`-aware `find_by_op_uuid`, the resumable
`ResumableKind::{Create, Update}` distinction, and the `about` quota
query). Don't duplicate it here.

- `GoogleDriveStore` — real impl, hits `googleapis.com`.
- `InMemoryRemoteStore` — keeps state in-process. Implements the same
  contract. Fast, deterministic, no network.

The `driven-core` crate accepts `Arc<dyn RemoteStore>` — never knows
which impl is in play.

### 14.3 Time, randomness, FS

All side-effects that we want determinism on are injected:
- `Clock` trait → `SystemClock` and `FakeClock` (manual tick).
- `Rng` trait → `OsRng` and `SeededRng`.
- Filesystem walking accepts a root path; tests build trees under
  `tempfile::TempDir` via the `driven-test-fixtures` helpers
  (e.g. `tree!("foo/bar.txt" = "hello", "foo/.gitignore" = "*.log")`).
- Network connectivity & battery state are abstracted behind traits in
  `driven-power`; the fake lets a test simulate going on battery
  mid-upload.

### 14.4 Snapshot tests

For the sync engine itself, we use snapshot testing via `insta`:
given an initial `(local_tree, remote_state)`, run one sync cycle,
assert the resulting `(remote_state, db_state)` matches a checked-in
snapshot. Trivial to update; trivial to detect a regression.

### 14.5 Real-Drive E2E

A small `tests/e2e_real.rs` test set:
- Skipped unless `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID`
  are both set. The token is the maintainer's own OAuth refresh token
  for a dev GCP project (minted via `driven-cli auth` + dumped via
  `driven-cli dump-refresh-token`, gated on `--features dev-tools`).
- Service account owns / is invited to a throwaway Drive shared drive.
- Each test creates a UUID-named root folder under that shared drive,
  exercises a real sync against it, asserts via the real Drive API,
  cleans up.
- Designed to be runnable from the agent's laptop and from CI. CI
  reads creds from a GitHub Secret.

### 14.6 Fast iteration

- `cargo watch -x 'test -p driven-core'` while editing core.
- `cargo tauri dev` for UI iteration; hot-reloads frontend.
- `just dev` recipe boots the full app against a pre-seeded fake Drive
  store + a pre-seeded local tree, so the agent can manually click
  through flows without ever touching real Google.

### 14.7 Stress and chaos harness

Layers 14.1-14.5 are necessary but not sufficient. A separate
workspace binary `crates/driven-chaos/` (sister project to the app)
runs adversarial fixtures the trait-seam fake cannot reach:
pathological filenames, NTFS reparse-point hazards, locked-file
soak, mid-sync mutation, Drive-side account state changes, kill -9
mid-pipeline. It boots the headless core (no Tauri), asserts on the
SPEC §24 error-code taxonomy, supports capability gating (SKIPPED
when a host lacks Admin / NTFS / real-Drive creds), and produces a
machine-readable + human report. CI runs three tiers - hermetic,
fake-drive with fault injection, and real-Drive (gated on M4) - plus
a weekly 6 h fuzz soak. Specified in `design/STRESS_HARNESS.md`;
landed at ROADMAP M3.7.

---

## 15. Build, sign, ship

GitHub Actions, three workflows:

| Workflow         | Triggers                            | Outputs                                                |
|------------------|-------------------------------------|--------------------------------------------------------|
| `ci.yml`         | PRs, pushes to `main`               | `cargo test`, `cargo clippy`, `cargo fmt --check`, `pnpm lint`, `pnpm test`, `cargo deny`. |
| `release-please.yml` | push to `main`                  | Maintains a release PR. On merge: tag + GitHub Release with notes. |
| `release.yml`    | Tag push `v*` (stable) + nightly cron (dev) | `tauri-action` builds Windows MSI, macOS DMG (universal), Linux AppImage + .deb, signs updater payload, uploads to GH Release, generates `update.json` for both channels and pushes to `gh-pages` branch / R2. |

V1 does **not** sign the installers themselves — only the updater payload
(required by Tauri's verifier). Users see SmartScreen / Gatekeeper warnings
on first install; the README has a 1-paragraph "how to bypass" section.

**macOS posture (V1):** the maintainer has no Apple hardware, so
macOS bundles ship unsigned and the in-app updater is **not expected
to work cleanly** on macOS in V1 (Gatekeeper + quarantine make
unsigned bundle swaps unreliable). macOS users do a manual reinstall
per release — the in-app updater UI surfaces "macOS: download the new
DMG from <releases URL>" rather than offering an install button.
Documented as a known V1 caveat; no fix planned until Apple hardware
is available.

**Windows posture (V1+):** unsigned for V1. When V1 distribution
warrants it, **Microsoft Trusted Signing via Azure (~$10/mo)** is the
likely upgrade path — supported by `tauri-action`, reputation builds
over time, no physical token. Until then, README documents the
SmartScreen click-through ("More info → Run anyway").

---

## 16. Key external libraries

(Anything not pinned to a version here is "latest stable at time of first
use" and gets pinned in `Cargo.toml`.)

| Crate                              | Version (target) | Purpose                                            |
|------------------------------------|------------------|----------------------------------------------------|
| `tauri`                            | 2.x              | Shell                                              |
| `tauri-plugin-autostart`           | 2.x              | Start at login                                     |
| `tauri-plugin-single-instance`     | 2.x              | Prevent duplicate launches                         |
| `tauri-plugin-updater`             | 2.x              | In-app updates                                     |
| `tauri-plugin-dialog`              | 2.x              | Native file / folder pickers                       |
| `tauri-plugin-opener`              | 2.x              | "Show in Finder / Explorer"                        |
| `tauri-plugin-notification`        | 2.x              | OS notifications                                   |
| `tauri-plugin-deep-link`           | 2.x              | Custom URI scheme handling (loopback OAuth aux)    |
| `specta` + `tauri-specta`          | 2.x              | TS type + typed-command generation from Rust IPC   |
| `tokio`                            | 1.x              | Async runtime                                      |
| `anyhow`                           | 1.x              | Error type at app boundary                         |
| `thiserror`                        | 2.x              | Library-crate error enums                          |
| `async-trait`                      | 0.1.x            | `RemoteStore` trait + others                       |
| `bytes`                            | 1.x              | Byte buffer for streaming pipelines                |
| `futures`                          | 0.3.x            | `Stream` trait for `UploadBody::Stream`            |
| `rayon`                            | 1.x              | CPU-bound parallel hashing + encryption            |
| `axum`                             | 0.8.x            | One-shot loopback HTTP server for OAuth callback   |
| `hyper`                            | 1.x              | (transitive; surfaced for connection-pool config)  |
| `hickory-resolver`                 | 0.24.x           | DNS probe + custom-resolver fallback (§5.8)        |
| `rustls-native-certs`              | 0.7.x            | OS trust store for TLS                             |
| `sqlx`                             | 0.8.x            | SQLite + compile-time-checked queries              |
| `reqwest`                          | 0.12.x           | HTTP client (uses rustls + native certs)           |
| `oauth2`                           | 5.x              | OAuth 2.0 + PKCE primitives                        |
| `sys-locale`                       | 0.3.x            | OS locale detection at boot                        |
| `keyring`                          | 4.x              | OS keychain (Secret Service on Linux)              |
| `ignore`                           | 0.4.26 ✓         | gitignore-aware walking (verified)                 |
| `notify`                           | 8.x              | Filesystem watching (DESIGN §5.9)                  |
| `dunce`                            | 1.x              | Windows path canonicalisation                      |
| `blake3`                           | 1.x              | Hashing + `derive_key` for filename sub-keys (`rayon` feature for big-file parallel hash) |
| `md5`                              | 0.7.x            | Drive `md5Checksum` (note: crate name is `md5`, import as `use md5;`) |
| `chacha20poly1305`                 | 0.10.x           | XChaCha20-Poly1305 + STREAM (content) via `aead::stream` |
| `bip39`                            | 2.x              | Recovery-phrase encoding / decoding of master key  |
| `windows`                          | 0.62.x           | Win32 bindings for VSS + sleep/wake (`Win32_Storage_Vss`, `Win32_System_Com`, `Win32_System_Power`, `Win32_UI_WindowsAndMessaging` features) |
| `zbus` (target=linux)              | 5.x              | systemd-logind DBus for sleep/wake events                                  |
| `objc2`, `objc2-app-kit` (target=macos) | 0.5.x       | NSWorkspace bindings for sleep/wake events                                 |
| `serde` + `serde_json`             | 1.x              | Serialization                                      |
| `tracing`                          | 0.1.x            | Structured logging                                 |
| `tracing-subscriber`               | 0.3.x            | Tracing subscriber + EnvFilter                     |
| `insta`                            | 1.x              | Snapshot tests                                     |
| `tempfile`                         | 3.x              | Temp dirs in tests                                 |

We do NOT use: `age` (was a wrapper around exactly the
`chacha20poly1305` STREAM construction we use directly now);
`yup-oauth2` (we use `oauth2` and a custom refresh wrapper);
`argon2` (recovery is BIP39, KDF is BLAKE3 `derive_key`); `online`
(§5.8 rolls its own probe topology); `starship-battery` (per-OS APIs
called directly); `wiremock` (the `InMemoryRemoteStore` fake is the
trait-seam test path; HTTP mocking is unnecessary).

Standard parallelism crate: `std::thread::available_parallelism()` is
preferred over the `num_cpus` crate for the worker-pool sizing math —
no extra dep needed.

Frontend:
- Vue 3.x, Vite 5.x, TypeScript 5.x, Pinia 2.x, Tailwind 3.x.
- `vue-i18n` 11.x + `@intlify/unplugin-vue-i18n` 11.x (Vite plugin, ICU MessageFormat).
- `@intlify/eslint-plugin-vue-i18n` 4.x (provides `no-raw-text` rule).
- `@tauri-apps/api` 2.x.
- Vitest 2.x + Vue Test Utils 2.x.
- Playwright 1.x (e2e UI tests against built bundle) - DEFERRED to V1.1; not a
  V1 dependency (see §14.1). In V1 the wizard e2e is the Vitest jsdom mount.
- ESLint 9.x + Prettier 3.x.

Dev tooling:
- `cargo-deny` 0.16.x (CI license + advisory check).
- `cargo-watch` 8.x (dev iteration recipe in `justfile`).

Rust-side i18n:

| Crate         | Version (target) | Purpose                                                  |
|---------------|------------------|----------------------------------------------------------|
| `rust-i18n`   | 3.x              | Tray menu + OS notification + autostart-launcher strings |

---

## 17. Open questions deferred to V2+

> NOTE (2026-06-25): several items once deferred here SHIPPED in **0.2.0** -
> **schedule windows** (#12), **pre/post backup shell hooks** (#16), and
> **bandwidth throttle by network type / metered** (#17) - plus **CLI local-state
> inspection** (`status`/`history`/`verify`, #13). They are annotated "(SHIPPED
> 0.2.0)" below; the surrounding design text is retained as historical context.
> Metered detection is real on all three desktop OSes (#32): Windows
> `INetworkCostManager::GetCost`, Linux NetworkManager `Metered` over D-Bus, and
> macOS `NWPathMonitor` (`isExpensive` / `isConstrained` as the metered proxy,
> since macOS exposes no literal "metered" bit).

- **rclone-crypt-format compatibility** as opt-in second format.
- **Block-level dedup** (CDC) for huge frequently-rewritten files.
- **Restore-by-date / point-in-time** (requires versioning).
- **Pre/post backup shell hooks.** (SHIPPED 0.2.0, #16)
- **Backends beyond Google Drive** (OneDrive, S3, Backblaze B2).
- **Schedule windows** (time-of-day rules, e.g. "only sync 23:00-06:00").
  (SHIPPED 0.2.0, #12 - time-of-day backup gating; the §3.5 "V1 does NOT ship
  this" decision was reversed post-GA. The original reservation note is kept for
  history: the DB column `schedule_json_v2_reserved` and a hidden Settings section
  were the V1 placeholder before the feature landed.)
- **Bandwidth throttle by network type** (slower on metered, full on
  unmetered). (SHIPPED 0.2.0, #17 - metered pause/throttle; real metered
  detection on Windows only, conservative "unmetered" default on macOS/Linux.)
- **macOS Spotlight integration** (`mdimport` of restored files).
- **Small-file bundling (tar.gz batches).** When a directory contains many
  small files, Drive's per-file API overhead dominates and bandwidth sits
  idle (each file create is ~1 RTT to Drive + the pacer's `2 files/sec`
  cap chews any large tree alive). Heuristic: any subdirectory whose
  `file_state` rows show median size below a configurable threshold
  (default 16 KiB) and count above another (default 200) gets packed into
  a single `.driven-bundle.<hash>.tar.gz` per scan. Restore must
  transparently unpack on download. Tradeoffs to design through:
  - Granular change detection inside a bundle requires either re-uploading
    the whole bundle on any inner file change (acceptable if the bundle
    is genuinely cold data, painful otherwise) or content-addressed
    chunking of the tarball itself (huge complexity, drifts toward
    restic-style architecture).
  - Drive UI browsability is lost — users can't see individual files
    without restoring through Driven.
  - Encrypted sources compose: the bundle is built first, then the whole
    bundle is encrypted as a single stream — simpler than per-file
    encryption inside the bundle.
  - This unifies well with the same machinery needed for backup
    versioning (M9 of the V1.x roadmap) and is the right time to revisit
    `RemoteEntry` storage policy as a per-source choice (`one-file-per-file`
    vs `bundle-cold-dirs` vs `chunked`).

---

## 18. Resolved defaults (gap fills from audit)

These were flagged as implementation gaps in the pre-implementation
audit and resolved here with concrete values so the implementing
agent has no ambiguity. All are user-overridable via Settings → Rules
unless noted.

### 18.1 AIMD pacer deltas
- **Decrease:** halve the per-second budget on any `429`,
  `403 rateLimitExceeded`, `403 userRateLimitExceeded` (already in §5.4).
- **Increase:** every 10 minutes of zero-throttle window, `+5` qps to
  the transaction bucket and `+1/s` to the file-create bucket. Capped
  at the user-configurable hard cap (default `200` qps, `50` files/s).
  Any throttle response resets the 10-minute window timer.
- **Reset on quota-reset boundary:** `403 dailyLimitExceeded` pauses
  the account until midnight Pacific (Drive's daily-quota reset);
  buckets re-initialise at the optimistic starting values (50 qps,
  10 files/s) when the pause lifts.

### 18.2 ThroughputProbe disk-saturation signal
Per-OS, sampled at 5-second intervals, in-process:
- **Linux:** parse `/proc/diskstats` deltas for the device backing the
  source root; "busy time" / wall-clock interval > 80 % → saturated.
- **macOS:** `IOKit` `IOBlockStorageDriver` `Statistics` dict, same
  ratio.
- **Windows:** PDH counter `\PhysicalDisk(_Total)\% Disk Time` > 80 %
  → saturated.
"Saturated" = "we are bottlenecked by the disk; reducing concurrency
won't help; raising it will hurt." Used by §11.4.7 adaptive
parallelism to keep the pool from raising when disk is the bound.

### 18.3 State DB backup cadence
Backup the SQLite state DB to `<Drive root>/_driven-meta/state-db/`:
- **Every 6 hours** during steady state (aligned with the updater check
  cadence to consolidate background work).
- **Immediately after any orchestrator cycle that mutated ≥ 1000 rows**
  (a large initial scan or a big delete-propagation pass).
- **At clean shutdown** (final flush before quit).
- **Skipped** when the local state.db hash hasn't changed since the
  last backup (cheap content-addressing).
- **Retention on Drive:** keep 7 most-recent daily backups + 4
  most-recent weekly backups (the weekly is the latest backup taken
  on a Sunday UTC). Older are trashed via the same 30-day Drive trash
  path as everything else.

### 18.4 Activity log retention + write batching
- **Insert batching:** writes coalesce on a 1-second window per
  source (the activity writer buffers events; flush is single
  transaction).
- **Background pruner:** runs hourly; deletes `ts < now - 30d`.
- **Hard cap:** 500 000 rows total. When exceeded, oldest rows pruned
  first regardless of age (catches the runaway-error case).
- **Post-prune:** `PRAGMA wal_checkpoint(TRUNCATE)` so the WAL doesn't
  carry the deleted pages forever.

### 18.5 Multi-user shared-machine policy
- **Per-OS-user, no cross-user sharing.** Each OS user runs their own
  Driven instance with their own `config_dir` (Tauri default per-user
  paths), their own SQLite state, their own keychain entries scoped
  to their OS user, their own backup sources, their own Drive accounts.
- **Startup conflict check:** on launch, Driven attempts to acquire an
  exclusive flock on `<config_dir>/driven.lock`. If another process
  holds it (e.g. user has two Driven installs against the same
  config_dir), the second exits with a clear error pointing at the
  first PID and `<config_dir>` path.
- **Shared / multi-tenant mode is NOT supported in V1.**
- A shared-state mode (multiple OS users backing up the same Drive
  account from one machine) is V2; documented as "not supported"
  rather than silently broken.

### 18.6 Initial-scan UX (huge first-time sources)
A 1 TB source's first scan + first upload takes hours and produces a
lot of confusing "is this working?" UX. Driven handles it by:
- **First-scan banner:** persistent across app restarts until any
  source has hit 99 % of its initial-upload byte total.
  ("Initial backup in progress — N% (X GB of Y GB), ~Z hours
  remaining at current speed.")
- **ETA derivation:** rolling 30-second throughput window; ETA =
  remaining_bytes / current_throughput.
- **Resumability:** the scanner persists its last-enumerated-path per
  source every 60 seconds into `backup_sources.last_enumerated_path`.
  On app restart mid-scan, the scanner resumes from the persisted
  path rather than re-walking from the root. The upload work in
  `pending_ops` is durable already (DESIGN §5.6).

### 18.7 Clock-change handling
Wall-clock can move backwards (DST, NTP correction, user manually
changes the clock). The scanner's mtime-based fast path must not be
fooled.
- **Store both monotonic and wall clock** alongside any "this happened
  at time X" record: `last_full_hash_at_monotonic_ns` + `last_full_hash_at_wall_ms`.
- **Decisions use `max(wall_delta, monotonic_delta)`** so a backwards
  wall jump doesn't make us think no time has passed.
- **On detected backwards wall jump > 60s** (compare current wall ms
  vs the last recorded wall ms): force the next scan to re-hash any
  file whose mtime falls within `[now - jump_size, now + 60s]`
  (defensive against the file being touched in the same window the
  clock moved).
- mtimes themselves are stored as-recorded; we don't try to "correct"
  them against the clock change.

### 18.8 IPC scalar validation
A `validate.rs` module in `src-tauri` defines const caps and a
single helper per argument shape:
- `search_files.query`: max 256 chars, must be valid UTF-8, must not
  contain raw newlines or NUL.
- Glob patterns (`include_patterns`, `exclude_patterns`): per-pattern
  max 512 chars, per-source max 256 patterns total; rejected if
  `ignore` crate's `Override` builder errors on them.
- `duration_secs`: 0..=31_536_000 (max 1 year).
- `limit` (paging): 1..=10_000.
- `page` (paging): 0..=u32::MAX, computed `offset = page * limit`
  bounded.
- `before_ts`: must be a reasonable UNIX-ms (positive, before
  `now + 1day`).
- All `PathBuf` args go through §11.6.1's validator.

Any failure surfaces as a structured IPC error with code
`internal.bad_request` and a hint pointing at the violated rule.

### 18.9 Reconcile-orphans algorithm
On startup, after the per-`pending_ops` reconciliation in §5.6:
- For each `backup_source`, **sample-list** the destination Drive
  folder (`list_folder(drive_folder_id)`) for any folder that had a
  `pending_ops` row in the last 24 h — full enumeration would be
  prohibitive for a 1 M-file source.
- For each returned remote entry, look up via
  `appProperties.driven.client_op_uuid`:
  - matches a `file_state` row → no action (healthy).
  - has `driven.source_id` matching but no matching `file_state` row →
    **orphan**, the result of a crashed create whose state-update
    never landed. Adopt into `file_state` if the local file still
    exists at the recorded relative path; otherwise trash it.
  - has neither attribute → **foreign** (user uploaded directly via
    Drive UI, or another tool); never delete, surface in Activity log
    once as informational.
- Additionally, every 100th sync cycle does a **deeper sweep** (one
  random subfolder of the source root, full listing) to catch
  longer-term drift.

---

## 19. Glossary

- **Source** — a `(local folder, Drive destination, account)` triple the
  user has configured for backup.
- **Account** — a Google account + its OAuth credentials.
- **Sync cycle** — one full scan → plan → execute pass.
- **Deep verify** — periodic full re-hash cycle (§5.2 step 4).
- **Drive trash** — Google Drive's native 30-day trash bin (`trashed=true`
  flag on the file).
- **Pacer** — the per-account token bucket that rate-limits requests to
  stay under Drive's quotas.
- **Orchestrator** — the per-account state machine that drives sync cycles.
- **Pending op** — a single queued unit of work (upload / trash / resume).
