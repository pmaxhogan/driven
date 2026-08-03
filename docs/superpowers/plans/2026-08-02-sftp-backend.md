# SSH (SFTP) Backup Destination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new backup destination over SFTP per spec `docs/superpowers/specs/2026-08-02-sftp-backend-design.md` (READ IT FIRST - it holds the locked design decisions; this plan holds the task order and anchors).

**Architecture:** New crate `crates/driven-sftp` (LocalFs-modeled store, S3-modeled config/keychain) + the three seams (BackendKind variant, driven-backend factory arms, RemoteStore impl) + IPC/UI wizard + chaos + container e2e.

**Tech Stack:** russh + russh-sftp (VERIFY current APIs via docs, not memory), keyring, existing driven-remote contract.

## Global Constraints

- ASCII "-" only, LF, conventional commit subjects; PR title `feat(core): SSH (SFTP) backup destination` (feat -> minor bump).
- Public repo: NEVER commit credentials; test keys/passwords must be obviously synthetic and generated in-test.
- Every keychain call in driven-sftp goes through a crate-private copy of `keyring_off_runtime` (copy verbatim from crates/driven-s3/src/config.rs:212-236 incl. its doc comment; a 3rd copy is the established pattern, do NOT extract a shared crate).
- The stored backend id string is `sftp` and is permanent once merged (stored-format stability, backend.rs:13-25).
- C5-P1-1 invariant: build_store NEVER silently falls back to a fake; missing credential => StoreOutcome::NeedsReauth.
- No new always-on tasks; connection lazily established.
- TDD per task; `cargo test --workspace`, clippy `-D warnings`, fmt, UI suite green at every task boundary.

## File Structure

- Create: `crates/driven-sftp/` (lib.rs, config.rs, store.rs, meta.rs, session.rs, error.rs, test-support server module), workspace member + Cargo.toml
- Modify: `crates/driven-remote/src/backend.rs`, `crates/driven-backend/src/lib.rs` (+Cargo.toml dep), `crates/driven-rclone/src/import.rs`, `src-tauri/src/commands/accounts.rs`, `src-tauri/src/commands/dtos.rs`, `ui/src/ipc/{types,commands}.ts`, `ui/src/stores/setup.ts`, `ui/src/views/SetupWizard.vue`, new `ui/src/components/SshCredentialsForm.vue`, `ui/src/locales/en-US.json`, `crates/driven-chaos/` (fixture + scenarios + registry), `crates/driven-e2e/src/scenarios/{mod.rs,sftp.rs}` + `flows.rs`, `Dockerfile` (e2e-runtime apt block ~:89-100), `Cargo.lock`

---

### Task 1: driven-sftp crate foundation (config + keychain + errors)

**Files:** Create `crates/driven-sftp/{Cargo.toml,src/lib.rs,src/config.rs,src/error.rs}`; add to workspace members.

**Interfaces produced:** `SftpConfig { host: String, port: u16 (serde default 22), root_path: String, username: String, auth: SftpAuthKind, host_key_fingerprint: Option<String> }` with `from_json/to_json` (mirror `S3Config`, crates/driven-s3/src/config.rs); `SftpCredentialStore::new(account_id)` with `load() -> Option<SftpCredential>`, `store(&SftpCredential)`, `purge()` - keychain service const `driven.sftp.credentials`, payload a serde-JSON blob `SftpCredential { auth: Password{password} | PrivateKey{pem, passphrase: Option} }`, hand-written Debug redacting all material, every keyring call wrapped in the crate-private `keyring_off_runtime` copy; `error.rs`: `sftp_error_classification` mapping table per spec (connect/timeout->Network, auth/hostkey->AuthInvalidGrant, ENOSPC-shaped->StorageQuota, else Other/Transient) producing `driven_remote::error::DriveError`.

- [ ] Step 1: failing tests - config JSON round-trip (default port), credential Debug redaction (assert secrets absent from format!("{:?}")), classification table (one case per class).
- [ ] Step 2: red. Step 3: implement. Step 4: `cargo test -p driven-sftp` + clippy + fmt green. Step 5: commit `feat(sftp): config, credential store and error mapping foundation`.

### Task 2: in-process SFTP test server + session layer

**Files:** Create `crates/driven-sftp/src/session.rs` + `crates/driven-sftp/src/test_support.rs` (cfg(test)/feature-gated `test-server`); dev-deps russh server features.

**Interfaces produced:** `test_support::TestSftpServer::spawn() -> { addr, host_key_fingerprint, root: TempDir, handle }` - a russh+russh-sftp SERVER accepting one configured user via password AND via a generated ed25519 keypair, serving a temp dir; fault hooks come later (chaos task) - keep this one honest. `session.rs`: `SftpSession::connect(&SftpConfig, &SftpCredential) -> Result<Self, DriveError>` doing TOFU pinning (record fingerprint if None in config at PROBE time only via an explicit `connect_and_pin` variant; plain connect hard-fails on mismatch), exposing an `sftp()` channel handle, lazy reconnect on broken channel.

- [ ] Step 1: failing tests - connect with password; connect with key; wrong password -> AuthInvalidGrant; pinned-fingerprint mismatch -> AuthInvalidGrant-classified error naming the fingerprints; connect to a dead port -> Network.
- [ ] Step 2: red. Step 3: implement (verify russh/russh-sftp current API via context7/docs.rs FIRST; document the version pinned). Step 4: green + clippy + fmt. Step 5: commit `feat(sftp): session layer with TOFU host-key pinning + test server`.

### Task 3: SftpStore RemoteStore implementation, part 1 (objects + folders)

**Files:** Create `crates/driven-sftp/src/{store.rs,meta.rs}`.

**Interfaces produced:** `SftpStore::new(config, credential) `; `RemoteStore` methods: ensure_folder / list_folder / create / update / metadata / download / trash+delete_permanent (aliases) / find_by_op_uuid. `meta.rs`: JSON sidecar per object (copy the shape of crates/driven-localfs/src/meta.rs Sidecar - read it first), sidecar name = `.<name>.driven-meta` in the same dir. Object id = root-relative path (LocalFs convention). Writes: remote temp name in the target dir -> SSH_FXP_RENAME -> re-download + re-hash verify (LocalFs read-back pattern; do NOT compare a hash you computed once against itself).

- [ ] Step 1: failing contract tests against TestSftpServer, mirroring the shape of driven-localfs's store tests (read crates/driven-localfs/src/store.rs test module first): create+metadata round trip incl. app_properties; folder marker semantics; update in place; download bytes match; trash removes object AND sidecar; find_by_op_uuid adopts; rename-into-place atomicity (temp name never visible in list_folder).
- [ ] Step 2: red. Step 3: implement. Step 4: green + clippy + fmt (workspace). Step 5: commit `feat(sftp): core RemoteStore object and folder operations`.

### Task 4: SftpStore part 2 (resumable, listing completeness, about)

**Files:** Modify `crates/driven-sftp/src/store.rs`.

**Also owed from Task 3 review (binding):** a stale-temp sweep mirroring localfs `sweep_stale_temp_files` (abandoned resumable temps otherwise accumulate invisibly - every listing filters them); a mutation-catching test for read-back verify (corrupt the committed bytes server-side between rename and verify -> create/update FAILS); `list_source_object_ids` must NOT inherit `live_annotated_files`' missing-dir->Ok(empty) mapping (missing source dir is an ERROR).

**Interfaces produced:** resumable_session/resume_chunk with opaque handle `driven-sftp:<base64 json>{temp_path, rename_to, size, digest_state}` - hydration of a foreign session stats + re-reads + re-hashes the remote temp file (LocalFs hydration pattern); list_source_object_ids walking the FULL source subtree - any readdir failure is an ERROR, never an empty/partial set (completeness invariant); about() via statvfs extension when offered else unknown.

- [ ] Step 1: failing tests - chunked upload resumes across a NEW SftpSession (simulated process restart); SessionInvalid on missing temp file; list_source_object_ids exact-set test + a readdir-error-fails test (delete perms on a subdir in the test server root); about() with/without statvfs.
- [ ] Step 2: red. Step 3: implement. Step 4: green. Step 5: commit `feat(sftp): resumable uploads, complete source listing and quota`.

### Task 5: the three seams (BackendKind + factory + rclone import)

**Files:** Modify `crates/driven-remote/src/backend.rs`, `crates/driven-backend/src/lib.rs` + Cargo.toml, `crates/driven-rclone/src/import.rs`.

**Interfaces produced:** `BackendKind::Sftp` (id "sftp", uses_oauth false, supports_folder_picker true, supports_version_history false) + ALL array + every const-fn arm (compiler enforces - the matches are deliberately exhaustive); `build_store` arm `build_sftp` (config from json, credential from keychain, NeedsReauth when absent - mirror build_s3 lib.rs:187-211); `purge_account_secrets` Sftp arm; `picker_root_id` arm; rclone `classify` maps `type = sftp` -> a supported import rendering host/port/user/path with credentials flagged re-enter-in-wizard; flip the `sftp -> None` expectation in `backend_kind_and_name_are_reported_for_every_outcome` (import.rs:1071-1085) to `Some(BackendKind::Sftp)`.

- [ ] Step 1: failing tests - ids_match_serde gains sftp; capability pin test (nondeterministic-create-key test gains the variant); build_store-with-no-credential -> NeedsReauth (mirror lib.rs:697-717 harness); purge deletes the entry (create then purge then load None); rclone classify sftp case.
- [ ] Step 2: red (several are compile errors from the exhaustive matches - that is the point; the assertion tests still must run red-then-green where expressible). Step 3: implement. Step 4: full `cargo test --workspace` + clippy + fmt. Step 5: commit `feat(core): register the sftp backend across the three seams`.

### Task 6: IPC create_sftp_account + DTOs

**Files:** Modify `src-tauri/src/commands/accounts.rs`, `src-tauri/src/commands/dtos.rs`.

**Interfaces produced:** `CreateSftpAccountRequest { host, port: Option<u16>, rootPath, username, auth: "password"|"privateKey", password: Option, privateKey: Option, passphrase: Option, displayName: Option }` (doc-marked IN-FLIGHT ONLY, never logged/echoed - copy the S3 request's doc block); command `create_sftp_account` mirroring `create_s3_account` (accounts.rs:1086-1209) EXACTLY in shape: probe first (connect_and_pin + write/remove round trip in root_path, off-thread), persist keychain -> AccountRow (kind Sftp, email label `{username}@{host}:{root_path}`), keychain rollback on row failure; response includes the pinned fingerprint for the UI to display.

- [ ] Step 1: failing tests mirroring the existing create_s3_account test harness in accounts.rs (find its tests first): probe-failure persists nothing (no keychain entry, no row); success persists both; row-failure rolls back keychain; label default + displayName override.
- [ ] Step 2: red. Step 3: implement (probe against TestSftpServer via the driven-sftp test-server feature as a dev-dep). Step 4: `cargo test -p driven-app` + clippy + fmt. Step 5: commit `feat(app): create_sftp_account command with probe-before-persist`.

### Task 7: wizard UI

**Files:** Create `ui/src/components/SshCredentialsForm.vue`; modify `ui/src/views/SetupWizard.vue`, `ui/src/stores/setup.ts`, `ui/src/ipc/types.ts`, `ui/src/ipc/commands.ts`, `ui/src/locales/en-US.json`; tests in `ui/src/__tests__/` (extend the setup-wizard + settings-components suites; new form needs its own mount test for the coverage gate).

**Interfaces produced:** `createSftpAccount(req)` IPC wrapper; setup store action mirroring `createS3Account` (setup.ts:216-260); SetupWizard step-2 becomes EXPLICIT branches (oauth / local / s3 / sftp - the current `v-else` catch-all must not swallow sftp) + `onSftpSubmit` + step-title branch; form fields per spec (auth-method toggle password|key+passphrase), presentational validation only; success surface shows the pinned fingerprint; locale blocks `backendPicker.kind.sftp.*` + `sftpSetup.*` (reuse the S3 trash/versioning warning copy pattern).

- [ ] Step 1: failing tests - form mount + submit payload shape; wizard renders the sftp branch when the sftp backend is selected (fixture: BackendDto id "sftp", usesOauth false); s3 fixture still hits the s3 branch (the catch-all regression test); store action success/error paths.
- [ ] Step 2: red. Step 3: implement. Step 4: full `pnpm --dir ui test:unit` + vue-tsc + lint + prettier. Step 5: commit `feat(ui): SSH (SFTP) destination setup flow`.

### Task 8: chaos tier

**Files:** Modify `crates/driven-chaos/` (new `sftp_fixture.rs` or module per existing layout - read scenarios/backends.rs first), extend `crates/driven-sftp/src/test_support.rs` with fault hooks.

**Interfaces produced:** fault hooks on TestSftpServer (mid-stream disconnect after N bytes, auth flap, host-key swap, ENOSPC on write, truncated-readdir - which MUST surface as a listing ERROR); `SftpFixture` mirroring `S3Fixture` (backends.rs:368-420); `InvariantSurface` impl for the sftp oracle (reporting.rs:104-119); Sftp arm added to `DestinationVanishedAcrossBackends` + registry entries for the sftp-specific scenarios.

- [ ] Step 1: failing scenario rows (hermetic tier). Step 2: red. Step 3: implement. Step 4: `cargo test -p driven-chaos` + the chaos hermetic binary run locally green. Step 5: commit `feat(chaos): sftp fault scenarios and cross-backend arm`.

### Task 9: container e2e + docs

**Files:** Modify `Dockerfile` (apt openssh-server + ssh-keygen -A in the e2e-runtime apt block ~:89-100), `crates/driven-e2e/src/scenarios/sftp.rs` (new), `scenarios/mod.rs` registry, `flows.rs` (`create_sftp_account` helper), `.claude/skills/driven-agent-qa/SKILL.md` + repo `CLAUDE.md` (mention the sftp scenario + stack).

**Interfaces produced:** `SftpStack::launch` mirroring S3Stack (s3.rs): spawns `/usr/sbin/sshd -D -p <free-port> -f <generated config>` as the non-root `driven` user (sshd_config with a test user, password auth on, host key pre-generated at image build), Skip-if-binary-missing via the `which()` pattern; `SftpRoundTrip` scenario: wizard-created sftp account -> add source -> sync -> byte-compare via docker-local paths -> driven-cli restore round trip; registered in `scenarios::all()` (no CI/workflow changes needed - run-all picks it up).

- [ ] Step 1: write the scenario (red locally = Skip without the binary; the container run is the real gate). Step 2: `just e2e-build` + `just e2e-run sftp-round-trip` in the container until PASS, then full `just e2e` (all scenarios) green. Step 3: docs updates. Step 4: commit `feat(e2e): sftp round-trip scenario with an in-container sshd`.

### Task 10: full green + final review + ship

- [ ] Full gates: cargo workspace test/clippy/fmt, UI suite, `just e2e` all green; chaos hermetic sweep.
- [ ] Final whole-branch review (most capable model) incl. ledger minors triage; fix wave if needed.
- [ ] Ship via `/d -mp`: PR `feat(core): SSH (SFTP) backup destination`, drive CI green, squash merge, merge the release PR, verify the release + updater endpoint.
