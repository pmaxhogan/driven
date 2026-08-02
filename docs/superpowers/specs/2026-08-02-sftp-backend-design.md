# SSH (SFTP) backup destination - design

Date: 2026-08-02
Status: authored autonomously under Max's standing goal ("add SSH/SCP remote
support", no human in the loop); decisions grounded in the 2026-08-02 backend
recon. Sequenced AFTER the agent-test-harness PRs merge so the test story can
target the new e2e infrastructure.

## Problem

Driven backs up to Google Drive, S3-compatible object stores, and local
folders. Users with a home server, NAS, or VPS want to back up over plain SSH
with nothing installed server-side. The kickoff request names SFTP explicitly;
"SCP" in the user's phrasing is colloquial for "over SSH" - raw SCP lacks the
rename/list/stat semantics the RemoteStore contract needs, so the protocol is
SFTP. (SMB/CIFS is out of scope for this feature; separate effort.)

## Shape: the three seams (design/DESIGN.md s4.2)

1. `BackendKind::Sftp` variant (stored/wire id `sftp`; UI label "SSH (SFTP)").
2. An arm in `driven_backend::build_store` (+ the MANDATORY
   `purge_account_secrets` arm - without it the keychain secret leaks on
   account deletion; recon flagged this as the one silent trap).
3. New crate `crates/driven-sftp` implementing
   `driven_remote::remote_store::RemoteStore`, modeled on `driven-localfs`
   (it is "a filesystem over the wire") with `driven-s3`'s network error
   discipline.

## Library choice

Native Rust, async: `russh` + `russh-sftp` (pure Rust, tokio-native, also
provides the SERVER side needed for the chaos fake). NOT rclone (the
driven-rclone crate is a config importer only; no backend shells out at
runtime) and NOT libssh2/ssh2 (blocking FFI in an async codebase).
Implementer MUST verify current russh/russh-sftp APIs via docs at build time,
not from model memory (repo rule).

## Capability flags (BackendKind const fns)

- `uses_oauth()` = false. `supports_folder_picker()` = true (SFTP readdir
  drives the same prefix-browse the S3 picker uses).
- `supports_version_history()` = false - SFTP writes to a deterministic
  `parent/filename` path, the same nondeterministic-create-key rationale that
  pins S3/LocalFolder to false (test
  `only_backends_with_a_nondeterministic_create_key_support_version_history`
  gains the variant). Revisit only with issue #220 (per-version objects).
- Trash: `trash()` == `delete_permanent()` (no remote undelete; the
  "simulated trash fills the destination" rejection from localfs/s3 module
  docs applies verbatim). Setup UI reuses the S3-style trash/versioning
  warning copy.

## Storage model on the remote

- Objects live under the account's configured `root_path` mirroring the
  LocalFs layout (folder markers, `driven.*` app_properties in a JSON sidecar
  companion per object - the `driven-localfs` `meta.rs` Sidecar pattern,
  since SFTP has no user-metadata headers).
- Integrity: write to a remote temp name, `SSH_FXP_RENAME` into place, then
  re-download and re-hash to verify (LocalFs's read-back pattern; closes the
  x==x checksum gap both existing backends document).
- Resumable uploads: LocalFs pattern, not S3 multipart - append to a remote
  temp file; `ResumableSession.url` carries an opaque
  `driven-sftp:<base64 json>` handle (temp path + expected digest + rename
  target); hydration of a foreign session stats and re-hashes the remote temp
  file rather than trusting bookkeeping.
- `list_source_object_ids` walks the source subtree fully (completeness-
  critical: a truncated listing reads as mass deletion upstream). Object id =
  root-relative remote path (stable, like LocalFs).

## Connection, auth, security

- Config (non-secret, `SftpConfig` JSON on the AccountRow): host, port
  (default 22), root_path, username, auth method tag, pinned host-key
  fingerprint.
- Secrets: ONE keychain entry, service `driven.sftp.credentials`, key =
  account id, value = small serde blob {auth: password|private_key,
  password?, private_key_pem?, passphrase?}; hand-written Debug redacts all
  material (S3 config.rs pattern).
- Host-key policy: TOFU-with-pinning. The creation probe records the server's
  host-key fingerprint into SftpConfig; every later connection hard-fails on
  mismatch with a dedicated classified error (surfaced like AuthInvalidGrant
  -> NeedsReauth-style attention, NOT silently retried). The wizard displays
  the fingerprint it pinned.
- Error mapping (`DriveError` classification): connect refused/timeout/reset
  -> Network; auth/host-key failures -> AuthInvalidGrant; remote disk full
  (SSH_FX_FAILURE on write with ENOSPC-shaped message or statvfs check) ->
  StorageQuota; other SFTP status codes -> Other/Transient5xx per retryability.
- Connection lifecycle: one multiplexed SSH session per store, lazy
  reconnect on drop; SFTP channel per operation batch. `about()` uses
  statvfs extension when the server offers it, else Unknown quota.

## Account creation flow

Mirror `create_s3_account` exactly: single IPC command
`create_sftp_account(CreateSftpAccountRequest)` (request DTO marked in-flight
only, never logged/echoed). Probe BEFORE persisting: full connect + auth +
host-key pin + write/fsync/remove round trip in root_path. Only then keychain
write -> AccountRow write, with keychain rollback via purge on row failure.
Account label (the reused `email` field): `{username}@{host}:{root_path}`,
overridable via displayName.

## UI

- BackendPicker: zero code changes (data-driven off descriptors); add
  `backendPicker.kind.sftp.*` locale keys.
- New `SshCredentialsForm.vue` (S3CredentialsForm model): host/port/root/
  username + auth-method toggle (password | private key paste + optional
  passphrase); presentational validation only; one submit -> the IPC command.
- SetupWizard step 2: the current `v-if oauth / v-else-if local / v-else s3`
  binary MUST become explicit branches (a third non-OAuth backend otherwise
  falls into the S3 arm) + `onSftpSubmit` + step-title branch.
- setup.ts store action `createSftpAccount` mirroring the S3 one.
- Fingerprint display on success (toast or wizard confirmation line).

## rclone importer

`import.rs::classify` gains `RemoteImport::Sftp` mapping rclone `type = sftp`
remotes (host/port/user/path; key_file/password flagged as
must-re-enter-in-wizard since rclone obscures passwords); flip the
`sftp -> None` case in `backend_kind_and_name_are_reported_for_every_outcome`
to `Some(BackendKind::Sftp)`.

## Testing

- Unit: config/keychain round-trip (redaction test), error classification
  table, sidecar encode/decode, session-handle encode/hydrate.
- The RemoteStore contract suite: run the same acceptance shape
  driven-localfs/driven-s3 use against a real in-process SFTP server (russh
  server side) - upload/list/rename/adopt-by-op-uuid/download/delete/
  list_source_object_ids completeness.
- Chaos: `SftpFixture` + `FaultySftpServer` (russh-based, the S3
  FaultyS3Server analogue: mid-stream disconnects, auth flaps, host-key
  swap, ENOSPC injection, truncated readdir MUST fail listing) implementing
  `InvariantSurface`; hand-add the Sftp arm to the cross-backend scenarios
  (DestinationVanishedAcrossBackends et al.) and register SFTP-specific
  scenarios.
- e2e: join whatever the agent-test-harness shipped - at minimum a
  `driven-cli` backup -> restore round trip against a containerized
  openssh-server (the harness's compose stack gains an sshd service), and a
  wizard walk in the WebDriver suite if that pillar landed.
- The three-seams exhaustiveness: compile-time (match arms) + the
  descriptor/ids_match_serde/purge tests extended.

## Out of scope

- SMB/CIFS (separate feature). Per-version objects (#220). Bandwidth
  scheduling changes (existing engine applies unchanged). rclone runtime
  usage. known_hosts file integration (pinning only, v1). Agent-forwarding /
  ssh-agent auth (v1 is password + pasted key; agent auth is a fast-follow
  candidate).

## Acceptance criteria

1. Wizard: create an SSH (SFTP) destination against a containerized sshd with
   password auth AND with key auth; bad credentials or unreachable host
   persist NOTHING (no keychain entry, no row).
2. Full backup -> verify -> restore round trip over SFTP via driven-cli in
   the container harness, byte-identical with --verify-against.
3. Host-key change after creation surfaces an attention state and blocks
   sync; re-accepting via account reconnect updates the pin.
4. Account deletion removes the keychain entry (purge test).
5. Chaos: the SFTP scenario set passes; cross-backend scenarios include the
   SFTP arm; a truncated listing fault fails the listing (never reads as
   empty).
6. `cargo test --workspace`, clippy -D warnings, fmt, UI suite + coverage
   gate green; conventional PR title `feat(core): SSH (SFTP) backup
   destination`.
