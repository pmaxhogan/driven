# Driven - Stress & Chaos Test Harness

> Companion to `DESIGN.md` / `SPEC.md` / `ROADMAP.md`. Read those first
> (specifically DESIGN §3.7 test strategy, §5 sync engine, §5.2.1
> symlinks/junctions, §5.3 open-file handling, §5.4 upload pipeline,
> §5.6 crash-safe execution, §5.8 network resilience, §11.4 concurrency,
> §14 testability; SPEC §1 layout, §3 `RemoteStore`, §8 executor identity
> checks, §23 workspace, §24 error taxonomy; ROADMAP M3.7).
>
> The harness is the sister project to the main app. It exists so that
> a single autonomous agent can answer "is Driven actually robust?" via
> `cargo run -p driven-chaos -- scenario run-all` and get a verdict,
> with zero user input. That autonomy requirement comes from DESIGN
> §3.7: *"everything must be testable by yourself without any input
> from me ... fast iteration."* Trait-seam unit tests cover correctness
> at the seam; this harness covers the adversarial fixtures the seam
> cannot reach.

---

## 1. Goals and non-goals

### 1.1 Goals

- **Catch the failure classes the user has lived with on Google Drive
  for Desktop and on other backup tools** - locked files, weird Unicode,
  Windows reparse-point indirection, mid-sync mutation, Drive-side
  account/quota state changes, kill-9 mid-pipeline.
- **Prevent regressions** of any class once caught - every reported
  bug becomes a scenario.
- **Exercise the paths the trait-seam fake cannot reach** - real disk
  ACLs, real reparse points, real ADS, real VSS, real OS handles held
  in write-exclusive modes by other processes. The fake satisfies the
  `RemoteStore` contract; this harness goes around it.
- **Give the maintainer one-command confidence before tagging.** A
  green `scenario run-all` (plus the manual smoke checklist) is the
  release gate.
- **Run autonomously.** No interactive prompts. Capability gaps
  (no Admin, no real-Drive creds, no big disk) yield SKIPPED rows,
  never blocked runs.

### 1.2 Non-goals

- **Kernel-level fuzzing.** No syscall fuzzers, no driver-level fault
  injection. We test against the OS as users have it.
- **Fuzzing the OAuth consent flow.** Interactive consent cannot be
  automated; covered by the manual `design/RELEASE_CHECKLIST.md` smoke.
- **Benchmarking vs other backup tools** (rclone, Restic, Kopia,
  Drive for Desktop). The harness asserts Driven's correctness, not
  comparative throughput.
- **Replacing unit / integration / e2e tests.** The harness is a third
  layer on top. Fast inner-loop tests stay in `cargo test`; the harness
  exists for cases that need fixtures expensive to set up or that need
  the whole orchestrator running for minutes.

---

## 2. Architecture

### 2.1 Crate

New workspace crate `crates/driven-chaos/`. Builds a single binary:

```
cargo run -p driven-chaos -- <subcommand> [args]
```

The crate depends on `driven-core`, `driven-drive`, `driven-crypto`,
`driven-power`, `driven-test-fixtures`. It does **not** depend on
`src-tauri/` - the whole point is to drive the headless core without
booting the Tauri shell. This is possible because of the thick-core /
thin-shell split in DESIGN §4.2: every subsystem the harness needs
(`Orchestrator`, `RemoteStore`, `Clock`, `PowerSource`, `StateRepo`)
is constructible without Tauri.

Dependency direction:

```
              src-tauri/                 driven-chaos
                  \                          /
                   \                        /
                    +------> driven-core <-+
                             driven-drive
                             driven-crypto
                             driven-power
                             driven-test-fixtures
```

### 2.2 Subcommands

```
driven-chaos fixture create <scenario>     # build the on-disk + remote fixture, leave it
driven-chaos fixture clean <scenario>      # tear down a scenario's persistent fixture
driven-chaos fixture clean --all
driven-chaos scenario list                 # print every scenario + requirements
driven-chaos scenario run <scenario>       # run one scenario end to end
driven-chaos scenario run-all              # run every scenario, respect capability gates
driven-chaos fuzz [--seed N --duration 1h] # property-style soak run
driven-chaos mutator <fs|drive> --while-syncing --scenario <name>
                                           # continuous-mutation soak
driven-chaos report [--format json|human]  # re-print last run's report
```

`fixture create` and `fixture clean` are split out from `scenario run`
because some fixtures (the 1 M-file tree, the 10 GB single file) take
minutes to materialise and should survive across runs. The fixture
directory defaults to `target/chaos-fixtures/<scenario>/` so cargo's
clean sweep blows it away if the user asks.

### 2.3 The `Scenario` trait

```rust
#[async_trait]
pub trait Scenario: Send + Sync {
    /// Stable kebab-case name. Used as the directory name under
    /// target/chaos-fixtures/, as the CLI argument, and in reports.
    fn name(&self) -> &'static str;

    /// Free-form one-liner for `scenario list` and the human report.
    fn description(&self) -> &'static str;

    /// What this scenario needs from the host. Missing capabilities
    /// produce a SKIPPED result with the list of missing items.
    fn requires(&self) -> CapabilitySet;

    /// Build the on-disk + remote-side fixture. May be a no-op if a
    /// cached fixture from a prior `fixture create` is still valid.
    async fn setup(&self, ctx: &mut ScenarioContext) -> Result<()>;

    /// Run assertions against a booted `DrivenHandle`. Returns a
    /// structured outcome (success, expected-graceful-failure with
    /// observed error code, unexpected behaviour).
    async fn run_assertions(&self, handle: &DrivenHandle) -> Result<Outcome>;

    /// Release filesystem handles, VSS snapshots, remote folders,
    /// keychain entries. Always called, even on assertion failure.
    async fn teardown(&self, ctx: &mut ScenarioContext) -> Result<()>;

    /// What the harness expects to observe. Drives the PASS / FAIL
    /// decision after `run_assertions` returns.
    fn expected_outcome(&self) -> ExpectedOutcome;
}
```

Most scenarios implement this trait directly. A handful (`fuzz`,
`mutator`) compose multiple scenarios programmatically and so are
implemented as harness drivers rather than `Scenario` impls.

### 2.4 `DrivenHandle`

```rust
pub struct DrivenHandle {
    pub state: Arc<StateRepo>,         // SQLite, hermetic per-scenario file
    pub remote: Arc<dyn RemoteStore>,  // InMemory by default, Google for capability-gated
    pub clock: Arc<dyn Clock>,         // FakeClock by default
    pub power: Arc<dyn PowerSource>,   // FakePower defaulted to "AC, unmetered"
    pub orchestrator: Arc<Orchestrator>,
    pub activity_tail: broadcast::Receiver<ActivityEntry>,
    pub error_tail: broadcast::Receiver<ErrorEntry>,
    pub config: HermeticConfig,
}
```

A handle boots one orchestrator wired to a hermetic `StateRepo`
(SQLite file under the scenario's tempdir), a `RemoteStore` chosen by
the scenario's `requires()`, and keychain entries scoped to a
`driven-chaos/<test-run-uuid>/` prefix so concurrent harness runs and
the user's real Driven install never collide. The handle exposes:

- `orchestrator.sync_now(source_id).await` - kick a cycle.
- `orchestrator.wait_for_state(matcher, timeout).await` - assert state
  transitions deterministically.
- `activity_tail.recv()` / `error_tail.recv()` - subscribe to events
  for assertions instead of polling SQLite.
- `kill_orchestrator().await` - simulate `kill -9` cleanly for
  crash-recovery scenarios.

### 2.5 Capabilities

Probed once at harness startup, cached for the run:

```rust
pub struct CapabilitySet {
    pub admin: bool,                  // Windows: elevated; Linux: euid 0 or CAP_SYS_ADMIN
    pub ntfs_volume: Option<char>,    // Windows: a drive letter that is NTFS
    pub case_sensitive_volume: Option<PathBuf>,
                                      // mountpoint of an ext4 / APFS-cs / NTFS-cs-flagged path
    pub free_disk_bytes: u64,
    pub real_drive_creds: bool,       // DRIVEN_E2E_REFRESH_TOKEN present + a throwaway folder ID
    pub vss_available: bool,          // Windows + admin
    pub long_paths_enabled: bool,     // Windows registry HKLM\...\LongPathsEnabled = 1
    pub network_reachable: bool,      // for the few scenarios that need real Internet
}
```

A scenario whose `requires()` is not satisfied gets SKIPPED with the
exact list of missing capabilities printed in the report. CI surfaces
SKIPPED as informational - never red.

---

## 3. Scenario catalogue

Naming convention: kebab-case, category prefix optional. Outcome
column uses the SPEC §24 error codes. **Codes prefixed `[NEW]` are
required additions to SPEC §24 that the harness motivates** - listed
in §10 of this doc; the harness does not silently widen §24.

Privilege column: `user` = any user, `admin` = elevated, `cap:X` = a
specific capability from §2.5.

### 3.1 Storage and disk

| Scenario                       | Setup                                                                                                                | Expected outcome (per SPEC §24)                                                                | Requires       |
|--------------------------------|----------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------|----------------|
| `disk-full-target`             | Mount a small (32 MiB) sparse loop / `VHD` and configure a source pointing inside it; fill to 0 free mid-sync.       | `[NEW] local.disk_full` surfaced + tray red + sync paused. No crash. Subsequent free-up resumes. | user           |
| `readonly-source-folder`       | Source folder marked read-only (Windows `attrib +R`; POSIX `chmod a-w`).                                             | Driven reads fine; new files uploaded. (Source is read-FROM, not written.)                      | user           |
| `readonly-file`                | One file inside an otherwise-normal source marked read-only.                                                          | Driven reads + uploads; status `synced`.                                                        | user           |
| `noaccess-file`                | A file stat-able but unreadable (Windows ACL Deny:READ for current user; POSIX `chmod 000`).                          | Windows: `local.io_error` logged per file. POSIX: `local.permission_denied` skip per file. Either way the scan continues and no delete of OTHER files cascades. | user           |
| `noaccess-folder`              | A subfolder we cannot enumerate (Windows ACL Deny:LIST; POSIX `chmod 0`).                                             | Walker yields `Err` on that subtree; per DESIGN §5.2 the deletion suppression must engage. **No file_state row under the unreadable subtree is enqueued for trash.** | user           |

### 3.2 File-size extremes

| Scenario                            | Setup                                                                                          | Expected outcome                                                                                                                                  | Requires                                  |
|-------------------------------------|------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------|
| `huge-file-10gb`                    | One 10 GB random-bytes file (deterministic via seeded RNG).                                    | Resumable upload completes; md5 matches; 3-stage pipeline (DESIGN §11.4.3) keeps all stages busy (idle < 10% wall-clock per stage).               | `cap:free_disk_bytes >= 20 GiB`           |
| `huge-file-50gb-mid-run-crash`      | One 50 GB file; `kill_orchestrator()` at 30% upload progress, wait 5s, re-boot handle.         | Reconciliation pass (DESIGN §5.6) finds the resumable session URL in `pending_ops`, resumes from saved offset, completes. No duplicate Drive object. | `cap:free_disk_bytes >= 60 GiB`           |
| `tiny-files-100k-in-one-dir`        | 100 000 files sized 0-1 KiB in one directory.                                                  | Completes. Pacer does not deadlock. Wall-clock recorded per run as a regression metric, not asserted (Drive RTT dominates).                        | `cap:free_disk_bytes >= 1 GiB`            |
| `million-files-nested`              | 1 000 subdirs x 1 000 files (0-4 KiB each).                                                    | Scanner memory bounded (RSS delta < 100 MiB during scan); SQLite state writes complete; FTS5 index builds without error.                          | `cap:free_disk_bytes >= 8 GiB`; cacheable |

The fixture for `million-files-nested` is the most expensive to
materialise (~ 15 min on a typical SSD). It is built once into
`target/chaos-fixtures/million-files-nested/` and re-used across
runs; `fixture clean million-files-nested` removes it.

### 3.3 Permissions and ACLs

| Scenario                       | Setup                                                                                                                 | Expected outcome                                                  | Requires                  |
|--------------------------------|-----------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------|---------------------------|
| `windows-acl-deny-read-file`   | NTFS ACL denying SYSTEM + Users READ on one file.                                                                     | `local.io_error` for that file; rest of source completes.          | Windows, `cap:ntfs_volume`|
| `posix-mode-000`               | Unix file with mode 0, owner = test user.                                                                              | `local.permission_denied` skip (warn) for that file - stands in for a macOS TCC/Full-Disk-Access denial; rest completes. | Linux or macOS            |
| `windows-acl-deny-enumerate`   | NTFS ACL denying LIST DIRECTORY on a subfolder.                                                                       | Walker `Err`; subtree delete-suppression engages (see DESIGN §5.2).| Windows, `cap:ntfs_volume`|
| `setuid-files`                 | Linux/macOS file with `chmod 4755`.                                                                                    | File uploaded as plain content. Bit not preserved on Drive - documented limitation, surfaced once per source in activity log. | Linux or macOS            |

### 3.4 Pathological filenames

One scenario per category. Each creates a file or directory using the
named glyph or sequence, asserts Driven backs it up, and (where the
restore-roundtrip flag is on) restores it via `RemoteStore::download`
and asserts byte-identity of name + content.

| Scenario                                   | Setup                                                                                                                                                     | Expected outcome                                                                                                                  | Requires                |
|--------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|--------------------------|
| `name-control-chars`                       | Files named with TAB, BEL (0x07), ESC (0x1B), DEL (0x7F). On Windows, CR (0x0D) / LF (0x0A) / CRLF are NTFS-illegal in filenames; on POSIX they're legal and we test them. | All upload, names round-trip byte-identical. Windows-illegal chars skipped on Windows with an `internal.bug` if attempted (they shouldn't be reachable). | platform-aware           |
| `name-rlo-zwj-zwnj-bom`                    | Names containing U+202E (RLO), U+200D (ZWJ), U+200C (ZWNJ), U+FEFF (BOM).                                                                                | Upload + round-trip; bytes identical.                                                                                              | user                    |
| `name-idn-homograph`                       | Two files in the same folder: `аpple.txt` (Cyrillic 'a' U+0430) and `apple.txt` (Latin 'a').                                                              | Both upload as two distinct Drive objects (Drive allows duplicate names; SPEC §3 documents this). No collision in `file_state` because path bytes differ. | user                    |
| `name-nfc-vs-nfd-cafe`                     | Two files: `café` written with NFC `e + U+0301` decomposed (NFD) and `café` precomposed (NFC). On case-sensitive volumes only.                            | Per DESIGN §5.2.3, the scanner NFC-normalises before lookup. If both forms map to the same NFC string, the harness expects `local.unicode_collision` surfaced and one of the two rejected at scan time. On case-insensitive Windows/macOS-default volumes only one of the two creations succeeds; harness adapts. | `cap:case_sensitive_volume` |
| `name-hangul-jamo`                         | A name like `한` written first as the precomposed Hangul syllable U+D55C, then as the decomposed jamo sequence U+1112 U+1161 U+11AB.                       | Same NFC-collision behaviour as above.                                                                                            | `cap:case_sensitive_volume` |
| `name-leaf-255-bytes`                      | Filename of exactly 255 UTF-8 bytes (the NTFS / ext4 leaf limit).                                                                                          | Upload succeeds; round-trip identical.                                                                                            | user                    |
| `name-path-4096-bytes`                     | Full path of approximately 4 KiB total via deep nesting.                                                                                                  | Upload succeeds on volumes with long paths enabled. On Windows without `LongPathsEnabled`, expect `local.path_too_long`.          | `cap:long_paths_enabled` (Windows) |
| `name-windows-reserved`                    | Files named `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, plus `CON.txt`. Creation fails on Windows; on POSIX they're legal and we test them. | On Windows: scanner skips with `local.io_error` (cannot open); on POSIX: uploads, round-trips. Note: restoring a POSIX-uploaded `CON` to a Windows machine fails - documented restore-side caveat. | platform-aware           |
| `name-trailing-space-and-dot`              | `foo .txt` and `foo.txt.`. Windows historically strips trailing space + dot at the Win32 API surface but the underlying NTFS file may exist.              | On NTFS via `\\?\` path prefix the names are preserved; via plain Win32 the names get mangled. Driven uses `\\?\` prefixes via `dunce`. Harness asserts the names round-trip. | Windows, `cap:ntfs_volume`|
| `name-separator-lookalike`                 | Names containing U+2215 DIVISION SLASH, U+FF0F FULLWIDTH SOLIDUS - resemble `/` but are not.                                                              | Upload; bytes identical; no spurious folder split.                                                                                | user                    |
| `name-unpaired-surrogate`                  | Filename containing an unpaired UTF-16 surrogate half (Win32 allows it via `\\?\`).                                                                       | Driven detects via UTF-8 conversion failure, logs `[NEW] local.invalid_filename`, skips the file, scan continues.                  | Windows, `cap:ntfs_volume`|
| `name-case-only-differs`                   | Two files in the same folder: `Readme.md` and `README.MD`.                                                                                                 | On case-sensitive volumes: two `file_state` rows, two Drive objects. On case-insensitive: only one of the two creations succeeds.  | `cap:case_sensitive_volume` |
| `name-normalisation-only-differs`          | Same NFC string, different forms. Covered by `name-nfc-vs-nfd-cafe` / `name-hangul-jamo`.                                                                  | See above.                                                                                                                         | `cap:case_sensitive_volume` |

### 3.5 NTFS / Win32 hazards

Derived from https://en.wikipedia.org/wiki/NTFS_links#Hazards plus
the broader reparse-point ecosystem. One scenario per hazard.

| Scenario                                   | Setup                                                                                                                                  | Expected outcome                                                                                                                                                                       | Requires                            |
|--------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|
| `hardlink-two-paths`                       | `mklink /H` two paths to the same inode under a source.                                                                                | Per DESIGN §5.2.1, each path uploaded independently (bytes duplicated on Drive). Two `file_state` rows. Documented.                                                                   | Windows, admin, `cap:ntfs_volume`   |
| `symlink-to-file`                          | `mklink` to a file under a source.                                                                                                     | Default: skipped per DESIGN §5.2.1 (don't follow). Per-source toggle changes behaviour; harness toggles both ways.                                                                    | Windows, admin                      |
| `symlink-to-directory`                     | `mklink /D` to a directory inside the source.                                                                                          | Skipped by default. With follow-symlinks ON: walker's cycle detection must engage if the link target is an ancestor.                                                                  | Windows, admin                      |
| `junction-mklink-j`                        | `mklink /J` to another folder (same volume).                                                                                            | Skipped by default (junctions are reparse-point category, treated like symlinks).                                                                                                     | Windows, admin                      |
| `reparse-point-non-symlink`                | A NTFS Dedup or Storage Replica reparse point (synthesised via `fsutil reparsepoint` if Dedup is not installed).                       | Read normally - OS handles indirection.                                                                                                                                               | Windows, admin, `cap:ntfs_volume`   |
| `onedrive-placeholder`                     | A file marked with `FILE_ATTRIBUTE_RECALL_ON_OPEN` (simulated via `SetFileAttributes`).                                                | Per DESIGN §5.2.1, scanner detects the attribute and skips by default; per-source "include cloud-only" toggle flips behaviour.                                                        | Windows                             |
| `recursive-junction-cycle`                 | Folder `A/B` contains a junction `loop` pointing at `A`.                                                                               | Walker's cycle detection engages; no infinite loop; finite completion. If the `ignore` crate ever stops detecting cycles, harness catches via 5-min timeout -> fail.                  | Windows, admin, `cap:ntfs_volume`   |
| `cross-volume-symlink`                     | Symlink under source on volume X points to file on volume Y. Then unmount Y.                                                            | Default: skipped (not followed). With follow ON + Y unmounted: `local.io_error` on read; no crash.                                                                                    | Windows, admin                      |
| `cross-volume-link-stale-after-reassign`   | Hardlink-style data ref on volume X; volume X re-lettered. Per the wiki: drive-letter-based links can dangle on reassignment.          | Driven does not chase drive-letter rewrites; affected paths surface as `local.io_error`. Documents the failure mode in the activity log.                                              | Windows, admin                      |
| `ads-alternate-data-stream`                | File `foo.txt` with an ADS `foo.txt:hidden`.                                                                                            | Main stream backed up; ADS lost. Surfaced once per source as a one-time activity-log notice (`[NEW] local.ads_skipped`).                                                              | Windows, `cap:ntfs_volume`          |
| `sparse-file`                              | A 1 GiB sparse file with only 4 MiB of allocated data via `fsutil sparse setflag`.                                                     | Driven uploads the logical content (zero ranges become real zeros on the wire). Documented size-on-Drive vs size-on-disk skew.                                                        | Windows, `cap:ntfs_volume`          |
| `compressed-ntfs-file`                     | File with NTFS compression on (`compact /c`).                                                                                          | Reads transparently as plaintext; uploads the decompressed bytes.                                                                                                                     | Windows, `cap:ntfs_volume`          |
| `encrypted-ntfs-efs`                       | EFS-encrypted file owned by current user.                                                                                              | Same-user: reads + uploads (the OS decrypts on read). Different-user (elevation path): `local.io_error`.                                                                              | Windows, `cap:ntfs_volume`          |
| `hidden-system-attributes`                 | File with `+H +S` attributes.                                                                                                          | Backed up normally. `ignore` crate honours `hidden(false)` setting (SPEC §6).                                                                                                          | Windows                             |
| `file-id-reuse-after-defrag`               | Synthetic: produce two `file_state` rows whose recorded inode / file-index later collide post-defrag.                                  | Documents that Driven keys local identity by `(source_id, relative_path)`, NOT by inode (see DESIGN §5.2.3). No misbehaviour. Asserted by snapshot diff. | Windows, admin                      |

### 3.6 Mutation patterns (soak)

These scenarios run a continuous mutator alongside Driven sync. Each
runs for a bounded duration (5 min default, configurable). Assertions
are eventual-consistency style: "after N more cycles past mutation
stop, Drive matches a snapshot of local taken at any moment after
mutation stop".

| Scenario                          | Setup                                                                                                                                        | Expected outcome                                                                                                                                            | Requires            |
|-----------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------|
| `frequent-edits`                  | One text file edited every 100ms for 5 min while sync runs.                                                                                  | No crash. No partial / corrupt upload (md5 always matches the bytes actually sent). Eventually consistent: post-mutation, Drive == local on the next cycle. | user                |
| `frequent-lock-unlock`            | One file locked / unlocked every 500ms via `CreateFile` with `DELETE` share-mode dropped.                                                    | `ERROR_SHARING_VIOLATION` handled gracefully, queued for retry, eventually succeeds. No retry-storm.                                                         | Windows             |
| `constantly-locked-db`            | A simulated PST: 100 MiB file opened with `GENERIC_WRITE | 0 (no share)` for the duration.                                                  | With Admin + `cap:vss_available`: backed up via VSS snapshot (per DESIGN §5.3). Without: `local.file_locked` + per-source `local.vss_unavailable` banner.    | Windows             |
| `truncate-and-rewrite`            | File rewritten via `O_TRUNC + write` every cycle (Word/Excel atomic-write pattern).                                                          | Per SPEC §8 pre/post `fstat` identity check: any mid-read mutation aborts with `local.file_changed_during_upload`, file re-queued, no false `synced`.        | user                |
| `append-only-log`                 | File appended 16 bytes every 200ms.                                                                                                          | Each upload contains a coherent snapshot (no torn writes). Later appends land on next cycle. Final state on Drive eventually matches local.                  | user                |
| `rename-storm`                    | Files renamed during scan window (rename every 1s).                                                                                          | V1 has no rename detection. Harness asserts: new path uploaded, old path trashed, no data loss. Documents the bytes-uploaded-twice cost.                     | user                |
| `editor-tilde-dance`              | Word/Photoshop pattern: write `~$foo.tmp`, rename over `foo`, delete tmp. Repeat every 2s.                                                   | **V1 has no built-in exclude defaults** (DESIGN §5.2 / SPEC §6 only honour gitignore + per-source patterns). Harness asserts current behaviour: tmp uploaded then trashed on rename. Documents a follow-up: a "common-editor-tmp" exclude preset would reduce churn (V1.x feature, not asserted here). | user                |
| `replace-via-atomic-rename`       | Other process atomically replaces a file mid-upload (writes to `.tmp`, renames over `foo`).                                                  | SPEC §8 inode-identity check fires: `local.file_replaced_during_upload`, re-queued. No partial upload commits to `file_state`.                              | user                |

### 3.7 Drive-side fuckery

These need either the extended `InMemoryRemoteStore` with the new
fault-injection methods (§5) or the real Drive with the maintainer's
E2E refresh token. Scenarios marked `cap:real_drive_creds` only run
in the `chaos-real-drive` CI job (and gated on M4 being landed - see
ROADMAP M3.7).

| Scenario                                          | Setup                                                                                                          | Expected outcome                                                                                                                                                       | Requires                       |
|---------------------------------------------------|----------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------|
| `dest-folder-deleted`                             | After initial sync, delete the destination Drive folder via the web UI (or simulated equivalent on the fake).  | Next sync: `ensure_folder` on the stored ID returns 404; `[NEW] drive.dest_folder_missing` surfaced; source halted; UI prompt "Re-pick destination?".                  | fake; optional `cap:real_drive_creds` |
| `access-revoked`                                  | User removes Driven at https://myaccount.google.com/permissions; refresh returns `invalid_grant`.              | `auth.invalid_grant` per SPEC §24; account marked `needs_reauth`; banner + OS notification per DESIGN §6.3.                                                            | fake; `cap:real_drive_creds` for the live path |
| `dest-folder-readonly`                            | Destination folder sharing changed to Viewer for the connected account; upload attempts.                       | Upload fails with 403; `[NEW] drive.dest_folder_permission_denied` surfaced; source halted with action prompt.                                                         | fake; optional `cap:real_drive_creds` |
| `dest-folder-moved`                               | Destination folder moved to a different Drive parent via the web UI.                                           | `file_id` is stable. Driven continues uploading; no error.                                                                                                              | fake; optional `cap:real_drive_creds` |
| `trash-emptied-with-our-file`                     | A file Driven owns is permanently emptied from Drive trash by the user.                                        | Next op on it returns 404; Driven detects, re-uploads (per `find_by_op_uuid` + reconciliation in DESIGN §5.6).                                                          | fake; optional `cap:real_drive_creds` |
| `storage-quota-mid-upload`                        | Fake injects `403 storageQuotaExceeded` after N bytes uploaded.                                                | Account halted; `drive.quota_exhausted` per SPEC §24; OS notification.                                                                                                  | fake                           |
| `daily-quota-exhausted`                           | Fake injects `403 dailyLimitExceeded`.                                                                          | `drive.daily_quota_exhausted` per SPEC §24; pacer pauses account until midnight Pacific per DESIGN §5.4 (`FakeClock` controls "midnight"); resumes correctly.            | fake                           |
| `concurrent-driven-instance-on-other-machine`     | Two `DrivenHandle`s share the same destination folder + account; each runs its own `client_op_uuid` series.    | Each instance's `find_by_op_uuid` adopts only its own creates. Foreign files (the other instance's) are NOT trashed even on the foreign-file delete-suppression path.   | fake                           |
| `drive-fileid-recycled`                           | Synthetic fake-only: after `trash`, the fake reuses the same `file_id` for the next `create`.                  | Driven detects via `appProperties.driven.client_op_uuid` mismatch and treats the reused id as foreign. No metadata bleed across files.                                  | fake                           |
| `concurrent-rename-on-drive`                      | User renames an uploaded file in the Drive web UI.                                                              | Driven's stored `file_id` still works for updates; local `relative_path` is authoritative. Rename ignored from Driven's perspective; next update overwrites Drive name. | fake; optional `cap:real_drive_creds` |

### 3.8 Concurrency edge

| Scenario                          | Setup                                                                                                                      | Expected outcome                                                                                                                                  | Requires |
|-----------------------------------|----------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------|----------|
| `pause-mid-resumable-5m`          | Pause Driven 5 min into a 10 GB resumable upload; `FakeClock.advance(5 min)`; resume.                                       | Session URL still valid (Drive sessions live 1 week); upload continues from saved offset.                                                          | fake     |
| `pause-mid-resumable-7d`          | Same as above but `FakeClock.advance(7 days)`. Driven persists session age, discards sessions older than 6 days.            | Stored session discarded; upload restarted from byte 0; no `drive.resumable_session_invalid` leaks past the executor.                              | fake     |
| `kill-9-mid-pipeline`             | `kill_orchestrator()` mid 16-file pipeline; reboot handle.                                                                  | Reconciliation pass (DESIGN §5.6) `find_by_op_uuid` adopts or trashes orphans; final state has no duplicates; `state.reconcile_orphan` logged.    | fake     |

### 3.9 Backend-specific destination hazards

Sections 3.1-3.8 all drive ONE destination: Google Drive, through
`InMemoryRemoteStore`. Driven now has more than one backend
(`driven-drive`, `driven-s3`, and the local-folder backend), and the
s6.3 invariants claim to be **backend-independent**. This section is
where that claim is tested rather than asserted: every row below ends
in the same `assert_invariants` sweep the Drive rows use, reached
through the backend-neutral `InvariantSurface` seam (s6.3).

Implemented in `crates/driven-chaos/src/scenarios/backends.rs`. Every
row is registered in BOTH `registry()` and `fault_injection_registry()`,
so the `chaos-fake-drive` job - the one that names itself after fault
injection - actually runs them.

#### Why these faults are injected on the WIRE

The S3 failure modes that lose data rather than merely erroring live
BELOW the `RemoteStore` trait, in the HTTP layer. A fault injected at
the trait seam arrives already classified and so proves nothing about
the code that does the classifying. So these rows drive the REAL
`driven_s3::S3Store` - real SigV4 signing, real `reqwest` round trips,
real XML parsing, real `classify_s3_response` - against the in-process
`FaultyS3Server` (s5.1), and inject faults as bytes on a socket.

| Scenario                                  | Setup                                                                                                | Expected outcome                                                                                                       | Requires |
|-------------------------------------------|------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------|----------|
| `s3-multipart-interrupted-between-parts`  | Connection dropped mid-`UploadPart`, with one part already committed.                                | Recovers to exactly ONE object whose bytes equal the local file. No gap where the lost part was, no duplicate.          | user     |
| `s3-multipart-interrupted-before-complete`| Connection dropped carrying `CompleteMultipartUpload`; every part is on the server.                  | NOTHING published and NOTHING recorded `synced` until a completion actually succeeds; then exactly one byte-exact object. | user     |
| `s3-complete-200-with-error-document`     | `CompleteMultipartUpload` answers HTTP **200** with an `<Error>` body and publishes nothing.          | The status is not trusted: no object, no `synced` row. Recovery afterwards yields exactly one byte-exact object.        | user     |
| `s3-slow-down-throttling`                 | A burst of `503 SlowDown` **longer than the finite 5xx retry budget** (`MAX_TRANSIENT_RETRIES = 6`). | The upload still completes - throttling retries indefinitely under the pacer. A shorter burst would not discriminate.   | user     |
| `s3-request-timeout-retryable-400`        | `400 RequestTimeout` mid-transfer.                                                                   | Retried despite the 4xx status; the file is not abandoned.                                                             | user     |
| `s3-kill-mid-upload-then-reboot`          | Upload cut, orchestrator dropped with no graceful shutdown, fresh handle over the same DB and bucket. | No orphaned object with no `file_state` row, no duplicate, nothing falsely `synced`.                                   | user     |
| `s3-destination-full-mid-upload`          | Byte budget exhausted with parts ALREADY uploaded (`QuotaExceeded`).                                  | `drive.quota_exhausted`; nothing partial published; freeing space lets the backup finish.                               | user     |
| `localfs-crash-between-temp-write-and-rename` | Two phases: a commit whose `rename` genuinely cannot land, then the abandoned temp file (holding the file's FULL bytes) a killed process leaves. | Nothing readable at the target path, no sidecar, nothing `synced`; the orphan is invisible to `list_folder` AND to the remote-existence audit; the stale one is swept, a FRESH one survives. | user |
| `destination-vanished-across-backends`    | Destination vanishes mid-cycle on ALL THREE backends from ONE body: a marker holding a different destination id (local folder), `NoSuchBucket` (S3), a latched flag (Drive). | `drive.dest_folder_missing`, nothing written after the destination went away, nothing already synced lost.  | user     |

#### The local-folder rows

The local-folder backend needs no protocol server: its destination IS a
directory, so `crates/driven-chaos/src/localfs_fixture.rs` constructs
each post-crash state byte-for-byte as the failure produces it, rather
than modelling it. Its three hazards:

- **Destination vanished mid-cycle** (the removable-media case, and the
  reason `.driven-destination.json` exists - an unmounted NAS mount
  point is an ordinary empty directory, so an existence check alone
  would write a whole backup onto the boot disk underneath the mount,
  where it disappears on the next remount while `file_state` still calls
  every file synced). Covered as the third arm of
  `destination-vanished-across-backends`. The injected fault is the
  NASTIER of the two the marker catches: not a missing marker, but one
  carrying a different destination id - a different stick at the same
  mount point, where the path exists, is a directory, is writable, and
  even looks like a Driven destination.
- **A crash between the temp-file write and the atomic rename**, covered
  by `localfs-crash-between-temp-write-and-rename`. Reached WITHOUT a
  fault seam in production code: phase 1 puts an object-shaped directory
  at the target path so the store really writes and `F_FULLFSYNC`es its
  temp file and the real `fsx::commit_rename` really fails; phase 2
  plants the temp file a killed process leaves. The row asserts on the
  DATA file's absence, never on a sidecar's presence: the backend
  commits data first and sidecar second on purpose, so a crash between
  them is benign by design and a row that asserted the opposite ordering
  would encode a bug as the expected behaviour.
- **Out of space (`ENOSPC`)** maps onto `DriveError::StorageQuota` - the
  local-folder errno table sends `ENOSPC`/`EDQUOT` there - the same
  variant S3's `QuotaExceeded` produces, so the end-to-end behaviour
  (account pauses, nothing partial published, recovery once space is
  freed) is covered mid-multipart by `s3-destination-full-mid-upload`.

  What is NOT covered is the localfs errno path itself end to end,
  because a real `ENOSPC` needs a real constrained VOLUME: unprivileged
  on macOS (`hdiutil`), root-only on Linux (`losetup`), admin-only on
  Windows (`New-VHD`). `disk-full-target` sets the precedent -
  capability-gated behind `DRIVEN_CHAOS_ALLOW_DISK_MOUNT` - and a row
  gated the same way would SKIP in the Windows-only PR gate, the one job
  that blocks a merge. The errno table is unit-tested in
  `driven-localfs`; what a chaos row adds beyond it is the orchestrator
  behaviour, which the S3 row proves. Recorded as a deliberate gap
  rather than a scenario that is always SKIPPED.

#### Every row must PROVE its fault fired, and must DISCRIMINATE

Two disciplines apply to every row in this section, both learned the
hard way:

- **Proof the fault fired.** The S3 server keeps per-fault firing
  counters and each row asserts on the one it armed; the local-folder
  rows assert an error was surfaced, and make their orphan-filtering
  claims against a destination that already holds a REAL object (on an
  empty destination "the audit returned nothing" is trivially true). A
  green run that never reached its fault is not a passing test - it is
  the `append-only-log` flake in a new costume.
- **Proof the row discriminates.** Each high-value row was re-run with
  the production defence it guards deliberately disabled, and must go
  RED. `s3-complete-200-with-error-document` needed all THREE of
  `driven-s3`'s independent defences removed before it failed (the body
  scan, the composed-ETag check, and the post-completion `HEAD`), which
  is worth knowing on its own. `s3-slow-down-throttling` FAILED this
  test as first written - one injected `SlowDown` passes whether or not
  the S3 code is special-cased, because a transient 5xx is also retried
  - and now injects a burst longer than the finite 5xx retry budget,
  which is the only shape that separates the two.

---

## 4. Continuous-mutation harness (the "mutator")

Lives in `crates/driven-chaos/src/mutator.rs`. Two flavours:

### 4.1 Filesystem mutator

Runs a mutation loop on a dedicated thread (not a tokio task - we
deliberately don't share the orchestrator's reactor, so OS scheduling
of the mutator is independent of Driven's I/O scheduling):

```rust
pub enum FsMutation {
    EditFile { path: PathBuf, every: Duration },
    LockUnlock { path: PathBuf, every: Duration },
    HoldLocked { path: PathBuf },           // for the duration
    TruncateRewrite { path: PathBuf, every: Duration, bytes: usize },
    AppendOnly { path: PathBuf, every: Duration, chunk: usize },
    RenameStorm { dir: PathBuf, every: Duration },
    EditorTildeDance { target: PathBuf, every: Duration },
    AtomicReplace { path: PathBuf, every: Duration },
}
```

Each variant maps to a soak-mode scenario in §3.6. The mutator
records every mutation with a wall-clock timestamp so post-run
assertions can correlate observed Driven behaviour with the mutation
timeline.

### 4.2 Drive-side mutator

Sends fault-injection commands to the fake remote (extending
`InMemoryRemoteStore` per §5 below). Used by the §3.7 scenarios and
by `fuzz` mode to inject failures at random points during a sync.

```rust
pub enum DriveMutation {
    InjectRateLimit { after_requests: u64 },
    InjectFiveHundred { after_requests: u64 },
    InjectInvalidGrant,
    InjectQuotaExhausted { after_bytes: u64 },
    InvalidateResumableSession { after_chunks: u32 },
    InjectMd5Mismatch { after_uploads: u32 },
    DropNetwork { for_duration: Duration },
    DeleteDestFolder,
    TrashOurFile { name_pattern: String },
}
```

### 4.3 `fuzz` mode

`driven-chaos fuzz --seed S --duration D` picks a random sequence of
filesystem and Drive-side mutations from a weighted distribution,
applies them against a synthesised source tree, runs Driven, and
asserts the post-conditions invariants from §6.3 (no data loss, no
infinite loop, no panic, no duplicate Drive objects keyed to the same
`client_op_uuid`). On any invariant violation, the seed + mutation
log is written to `target/chaos-fuzz-failures/<seed>.json` so the
failure is bit-reproducible.

The seed defaults to `now()` if not provided; CI's weekly soak run
uses `$(date +%s)` so the seed is recorded.

---

## 5. Drive-side fault injection (extending `InMemoryRemoteStore`)

The existing fake (per SPEC §3 / ROADMAP M1) already supports
`with_rate_limit_after(n)`. The harness extends it - the additions
live in `crates/driven-drive/src/fake/fault_injection.rs` so the
fake stays in `driven-drive` (not duplicated in `driven-chaos`) and
unit tests can use the same API.

```rust
impl InMemoryRemoteStore {
    pub fn with_network_drop_after(self, n: u64) -> Self;
    pub fn with_5xx_after(self, n: u64) -> Self;
    pub fn with_invalid_grant_after(self, n: u64) -> Self;
    pub fn with_quota_exhausted_after(self, n_bytes: u64) -> Self;
    pub fn with_session_invalidated_after(self, n_chunks: u32) -> Self;
    pub fn with_md5_mismatch_after(self, n: u64) -> Self;
    pub fn with_dest_folder_missing(self) -> Self;
    pub fn with_dest_folder_readonly(self) -> Self;
    pub fn with_update_not_found(self) -> Self;
    pub fn with_source_listing_broken(self) -> Self;
    pub fn with_fileid_recycle(self) -> Self;
}
```

Each scenario in §3.7 binds one or more. The fake's underlying
counter machinery is shared across builders so combinations behave
predictably (e.g. "5xx after request 50, then OK").

One RUNTIME counterpart exists alongside the construction-time
builders: `latch_dest_folder_missing(&self)`, in the same shape as the
already-present `arm_session_invalidated_after`. "The destination
vanished" is a mid-run event, and asserting that nothing already backed
up is lost needs a healthy first cycle to establish a synced baseline
before the fault arrives - which a construction-time-only surface
cannot express without rebuilding the store and discarding the very
objects the assertion is about.

### 5.1 S3-side fault injection (the in-process S3 server)

The S3 backend's interesting failures live **below** the `RemoteStore`
trait, so they cannot be injected the way §5 injects Drive's. A fault
introduced at the trait seam arrives already classified, which proves
nothing about the code that does the classifying - and
`classify_s3_response` is exactly the code that decides whether a
throttled backup keeps retrying or strands the file.

`crates/driven-chaos/src/s3_server.rs` therefore runs a
`FaultyS3Server`: an in-process, loopback-bound HTTP/1.1 server speaking
the S3 subset `driven-s3` uses, with faults injected as bytes on the
socket. The §3.9 rows drive the REAL `driven_s3::S3Store` against it -
real SigV4 presigning, real `reqwest` round trips, real XML parsing,
real classification.

**Why in-process rather than MinIO behind a proxy.** `driven-s3`'s own
integration suite already covers real MinIO and real Cloudflare R2. The
chaos jobs are a different question: per §7 they run **Windows-only** on
every PR, and `minio` is installed only on the Linux legs of `ci.yml` /
`coverage.yml`. A MinIO-gated chaos row would SKIP in the one job that
blocks a merge. An in-process server needs no external binary, no port
coordination beyond `:0`, and behaves identically on all three
platforms.

**No SigV4 validation, deliberately.** `S3Store` signs every request as
a presigned URL, so the credential rides in the query string and the
server never has to verify it. Validating signatures would test
`rusty-s3` rather than Driven.

The arming surface (runtime rather than construction-time, because an S3
row usually needs a clean first cycle and the fault only for the
second):

```rust
impl FaultyS3Server {
    // Throttling and transient failures with S3's own status quirks.
    pub fn arm_slow_down_after(&self, n: u64);          // 503 SlowDown, single shot
    pub fn arm_slow_down_burst(&self, n: u64);          // 503 SlowDown x n
    pub fn arm_request_timeout_after(&self, n: u64);    // retryable 400
    pub fn arm_internal_error_after(&self, n: u64);     // 500 InternalError

    // Destination-level, latching.
    pub fn arm_bucket_missing(&self);                   // 404 NoSuchBucket
    pub fn arm_access_disabled(&self);                  // 403 AllAccessDisabled
    pub fn arm_quota_bytes(&self, n: u64);              // QuotaExceeded past a budget

    // Multipart, the transport-level cuts with no trait representation.
    pub fn arm_drop_part_after(&self, n: u64);          // connection dies mid-UploadPart
    pub fn arm_drop_complete(&self, n: u64);            // dies at CompleteMultipartUpload
    pub fn arm_complete_error_document(&self, n: u64);  // HTTP 200 + <Error> body
    pub fn arm_no_such_upload_at_complete(&self);       // 404 NoSuchUpload

    // Race-window widening, and a reset.
    pub fn arm_response_delay(&self, delay: Duration);
    pub fn clear_faults(&self);
}
```

**Every row must prove its fault fired.** The server keeps per-fault
firing counters (`RequestCounts::slow_down_fired`,
`complete_error_documents_sent`, `quota_refusals`, ...) and each row
asserts on the one it armed. This is the §7 discipline applied to fault
injection: a green run that never reached its fault is not a passing
test, it is a test that did nothing - the exact shape of the
`append-only-log` flake.

**A burst is sometimes required for the row to discriminate.** One
`503 SlowDown` proves nothing, because a transient 5xx is also retried:
the row passes whether or not the S3 code is special-cased. Only a burst
LONGER than the executor's finite 5xx budget (`MAX_TRANSIENT_RETRIES`)
separates the two - rate limiting retries indefinitely and completes, a
transient 5xx gives up and strands the file.

**Real S3 semantics the server keeps**, because a fault row is only
meaningful if the non-faulted path behaves like S3: `Content-MD5` is
verified and a mismatch answered with `400 BadDigest`; a single-`PUT`
ETag is the content md5 while a multipart ETag is
`md5(concat(part md5s))-N`; `CompleteMultipartUpload` assembles ONLY the
parts the request body names and discards any other part held for that
upload id (the property the crash-resume path relies on);
`ListObjectsV2` honours `max-keys` and emits `NextContinuationToken`; a
`DeleteObject` of a missing key answers 204 while a `HEAD` of one
answers 404.

### 5.2 Local-folder fault construction (no seam needed)

The local-folder backend's destination IS a directory, so the harness
needs no protocol server and no fault seam in production code: it
constructs the post-crash on-disk state byte-for-byte and reads the
ground truth back with `std::fs`.
`crates/driven-chaos/src/localfs_fixture.rs` provides:

```rust
impl LocalFsFixture {
    pub fn swap_marker_identity(&self);                     // a different volume, same path
    pub fn block_target_path(&self, encoded: &str);          // the commit's rename cannot land
    pub fn plant_orphan_temp_file(&self, bytes: &[u8], age: Duration);
}
```

`block_target_path` is the interesting one: putting an object-shaped
DIRECTORY at the target path means the store really writes and
`F_FULLFSYNC`es its temp file and the real `fsx::commit_rename` really
fails, so the row reaches the instant between the sync and the rename
without any production code knowing it is under test.

`LocalFsOracle` is the fault-free listing, and it must reproduce the
backend's own notion of what is NOT an object - `.driven-meta/`,
`.driven-destination.json`, `.driven-tmp-*`, and the macOS AppleDouble
`._*` shadows the OS writes on exFAT/FAT32. It therefore delegates to the
backend's own `layout::is_control_entry` rather than re-deriving the
list, so the two cannot drift. (Counting a `.driven-tmp-*` file as an
object is precisely the bug the crash row hunts, and `._*` files never
appear on a tempdir - which is exactly why an oracle that forgot them
would look correct in CI and lie on real hardware.)

---

## 6. Reporting

### 6.1 Per-scenario verdict

```rust
pub enum Verdict {
    Pass { duration: Duration },
    Fail {
        duration: Duration,
        observed_outcome: Outcome,
        expected_outcome: ExpectedOutcome,
        diff: String,
    },
    Skipped { missing_capabilities: Vec<String> },
    Flaky { retried: u32, eventual: Box<Verdict> },
}
```

`Flaky` is reserved for soak scenarios where the harness explicitly
retries with extended timeouts; non-soak scenarios never produce
`Flaky` - either they're deterministic or they're a bug.

### 6.2 Output

**JSON**, one line per scenario:

```jsonc
{
  "scenario": "huge-file-50gb-mid-run-crash",
  "verdict": "pass",
  "duration_ms": 184310,
  "capabilities_used": ["free_disk_bytes>=60GiB"],
  "observed_outcome": {
    "error_codes_seen": ["state.reconcile_orphan"],
    "final_drive_object_count": 1,
    "final_blake3_matches_local": true
  },
  "log_excerpt_path": "target/chaos-runs/<run-id>/huge-file-50gb-mid-run-crash.log"
}
```

**Human**, after the JSON stream:

```
chaos run 2026-06-21T13:04:11Z, host=DESKTOP-ABC, capabilities=Admin,NTFS,VSS,real-Drive

 24 PASS    8 SKIP    0 FAIL    0 FLAKY    of 32 total

 SKIPPED (8):
   - cross-volume-symlink           (Linux only; this host is Windows)
   - posix-mode-000                  (Linux only; this host is Windows)
   - ...

 (all PASSES shown below collapsed; expand with --verbose)
```

On FAIL, each failure gets a detail block (expected vs observed,
relevant log excerpt, fixture path so the maintainer can poke).

### 6.3 Invariants asserted across every scenario

These run as cross-cutting post-conditions, separate from each
scenario's own assertions. A scenario can pass its own assertions
yet fail one of these.

- **No panic.** A panic in any subsystem fails the run.
- **No data loss.** For every `file_state` row with `status='synced'`,
  the recorded Drive object exists, its md5 matches, and the bytes
  hash to the recorded `hash_blake3`.
- **No infinite loop.** Each scenario has a hard wall-clock cap;
  exceeded -> FAIL with `harness.timeout`.
- **No duplicate Drive objects for the same `client_op_uuid`.** A
  scan of the final remote state must not show two objects sharing
  one `appProperties.driven.client_op_uuid`.
- **No `pending_ops` leak.** Post-run, `pending_ops` is either empty
  or only contains rows whose `scheduled_for` is in the future (a
  legitimate backoff).

These invariants are **backend-independent**, and that is enforced
structurally rather than by convention. `assert_invariants` is generic
over one narrow trait, `InvariantSurface`, which every backend
implements:

```rust
#[async_trait]
pub trait InvariantSurface: Send + Sync {
    async fn invariant_listing(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>>;
    async fn retained_content(&self, id: &str) -> Option<ObjectContent>;
}
```

Two properties of that seam are load-bearing:

- **It is fault-free.** A scenario that ends with a LATCHED fault
  (`auth.invalid_grant`, `NoSuchBucket`) would make the sweep's own read
  fail, and the harness would report "could not verify" as though it
  were "verified nothing wrong". Every implementation reads the
  destination's ground truth directly, bypassing the fault layer.
- **It is not `RemoteStore`.** On S3, `list_folder` returns keys, sizes
  and ETags but NO user metadata, so a duplicate-`client_op_uuid` check
  written against it would be silently vacuous on that backend - always
  green, never looking. It is also non-recursive on both backends.

There is therefore exactly ONE implementation of each invariant, shared
by every backend. A second, forked copy of "no duplicate
`client_op_uuid`" per backend is precisely the drift this section exists
to prevent.
- **No `unwrap` / panic in logs.** `tracing` captures must not
  contain `'thread .* panicked'`, `internal.bug`, or
  `Result::unwrap()` failures.

---

## 7. CI integration

Four jobs, all in `.github/workflows/chaos.yml`; the weekly soak is
NOT one of them (see below).

| Job                  | Trigger                              | Runtime  | What runs                                                                                                                                            | Gates merge? |
|----------------------|--------------------------------------|----------|------------------------------------------------------------------------------------------------------------------------------------------------------|--------------|
| `chaos-hermetic`     | Every PR/push (Windows-only; 3-OS on a `v*` tag) | ~5 min   | Every scenario whose `requires()` doesn't include `cap:real_drive_creds` AND doesn't require Admin (so it runs on standard GH runners).             | Yes          |
| `chaos-fake-drive`   | Every PR/push (Windows-only; 3-OS on a `v*` tag) | ~10 min  | Adds the fault-injection scenarios (§3.7 / §5) against `InMemoryRemoteStore`.                                                                       | Yes          |
| `chaos-real-drive`   | `v*` tag pushes only (no nightly schedule) | ~30 min  | The subset of §3.7 marked `optional cap:real_drive_creds` runs against real Drive using the maintainer's E2E refresh token + a throwaway folder. Skips cleanly (never fails) if the secrets are absent. | Tag-blocking |
| `chaos-soak`         | NOT run in CI - local/on-demand `just chaos-soak` | 6 h      | `driven-chaos fuzz --duration 6h --seed $(date +%s)` against `InMemoryRemoteStore`. A bounded `fuzz-smoke` row still runs per-PR as part of `chaos-hermetic`. | N/A (local task) |

**M4 gate: flipped.** The `chaos-real-drive` job depends on
`GoogleDriveStore` (M4) and the `DRIVEN_E2E_REFRESH_TOKEN` /
`DRIVEN_E2E_DEST_FOLDER_ID` / `DRIVEN_OAUTH_CLIENT_SECRET` secrets. It
was configured at M3.7 as `if: false`-skipped pending M4; M4 landed
long ago and the job now runs for real
(`if: startsWith(github.ref, 'refs/tags/')`), degrading to a clean
skip rather than a failure if a secret is missing. Kept here as a
historical note so M3.7 isn't self-contradictory.

**Admin / NTFS coverage.** GH's standard `windows-latest` runner does
not run elevated by default. The harness scenarios needing Admin
(VSS, junctions, hardlinks, ACLs) get exercised in `chaos-hermetic`
via `actions/runner` self-elevation patterns where possible, and via
a dedicated `chaos-windows-admin` job on a self-hosted Windows runner
that the maintainer provisions when reliable VSS coverage in CI
becomes the gating concern. Until then, those scenarios are SKIPPED
in CI and exercised locally on the maintainer's Windows machine. The
SKIPPED count is surfaced on the PR check so regressions in the
capability gate itself are visible.

---

## 8. Privilege and platform requirements

Most scenarios run as a normal user with no special setup.

- **Admin / elevation required**: every NTFS-link, junction, ACL, VSS,
  EFS, reparse-point, and ADS scenario. Documented per row in §3.5.
- **Big disk required**: `huge-file-10gb` (20 GiB free),
  `huge-file-50gb-mid-run-crash` (60 GiB free),
  `million-files-nested` (8 GiB inode + content). The
  `disk-full-target` scenario uses a small loop-mounted file (Linux),
  a `New-VHD`-mounted virtual disk (Windows), or an `hdiutil`-mounted
  sparse bundle (macOS) sized at 32 MiB. We do **not** actually fill
  the dev machine's disk.
- **Real Drive creds required**: §3.7 rows marked
  `cap:real_drive_creds`. Locally: `.env.test` with
  `DRIVEN_E2E_REFRESH_TOKEN` + `DRIVEN_E2E_DEST_FOLDER_ID`. CI:
  GitHub Actions secrets.
- **Big fixtures are cached**: `million-files-nested` and
  `huge-file-10gb` cache in `target/chaos-fixtures/` between runs.
  Cache key includes the scenario's fixture-version constant so
  bumping it forces a rebuild.

---

## 9. Failure exit codes and PASS semantics

Process exit codes from `driven-chaos`:

- **0** - every scenario PASS or SKIPPED.
- **1** - one or more scenarios FAIL.
- **2** - the harness itself errored (couldn't materialise a fixture,
  capability probe crashed, real-Drive creds present but unauthorised,
  etc.).

A scenario whose `expected_outcome()` is
`ExpectedOutcome::GracefulFailureWith { code: "drive.quota_exhausted" }`
**PASSES** if and only if:

- Driven emits that exact error code (SPEC §24 stable code, not
  message text) at least once;
- Driven does not crash or panic;
- The cross-cutting invariants (§6.3) hold - in particular, no data
  loss for files that completed before the failure;
- The orchestrator transitions to the documented post-failure state
  (e.g. `Paused { reason: AccountQuotaExhausted }`) within 5s of the
  injected fault.

Asserting on the stable error code rather than message text is what
makes the harness regression-safe - error messages can be reworded
without breaking tests.

---

## 10. New SPEC §24 error codes the harness motivates

The catalogue references codes that don't yet exist in SPEC §24.
**These additions are required for the harness to be implementable.**
They are not silently added to §24 here; the maintainer adds them as
part of landing M3.7, after reviewing this list.

| Code                                | Meaning                                                                                                              |
|-------------------------------------|----------------------------------------------------------------------------------------------------------------------|
| `local.disk_full`                   | Local volume reached 0 free; surfaced once per source, tray red, sync paused.                                        |
| `local.invalid_filename`            | Filename bytes are not valid UTF-8 (unpaired surrogate halves on Windows); file skipped, scan continues.             |
| `local.ads_skipped`                 | A file had one or more NTFS Alternate Data Streams that Driven did not back up; surfaced once per source.            |
| `drive.dest_folder_missing`         | Destination folder ID returned 404 on `ensure_folder`; source halted pending user re-pick.                           |
| `drive.dest_folder_permission_denied` | Destination folder's permissions changed; current account can no longer write; source halted.                      |
| `harness.timeout`                   | Cross-cutting: a scenario exceeded its wall-clock cap. Internal to the harness; never surfaced to users.             |

For each, a corresponding `local::` / `drive::` enum variant in the
appropriate crate's `thiserror` enum. The IPC layer translates to the
stable JSON code per SPEC §24's existing pattern.

---

## 11. Open questions deferred

- **Continuous-mutation harness on macOS.** V1 of the harness covers
  Windows + Linux for the mutation patterns. macOS gets the subset
  that doesn't need junctions, VSS, or `FILE_SHARE_DELETE` semantics.
  A future iteration with APFS clones could exercise the VSS-analogue
  story on macOS (V2 of the harness, paired with V2 of the locked-file
  story in DESIGN §5.3).
- **Real disk-full on dev hardware.** V1 uses sparse-file / loop
  / VHD mounts at 32 MiB. A full real-disk fill test is a manual
  pre-release smoke step that the maintainer runs on a scratch
  partition before tagging.
- **Self-hosted Windows runner for Admin / VSS in CI.** Currently
  SKIPPED in CI; exercised locally. The gate flips when a runner is
  provisioned.
- **Property-style coverage tracking.** `fuzz` currently records seeds
  for failures; it does not yet track which scenario categories the
  fuzzer has actually exercised. A coverage map (categories x
  mutation kinds x error codes seen) would tell us "the fuzzer has
  never touched this corner" - V1.x feature.
