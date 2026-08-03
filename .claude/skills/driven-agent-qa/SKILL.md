---
name: driven-agent-qa
description: >
  How an AI agent tests Driven end to end with minimal human QA: the
  containerized app-level e2e harness (real Linux desktop app under
  WebDriver), the fault-injection seams (DRIVEN_DATA_DIR, fault plans,
  e2e hooks, toxiproxy/iptables), the visual-regression suite, and the
  headless CLI round-trip tools. Use whenever a task involves verifying app
  behavior end to end, reproducing a bug in a controlled environment,
  checking for visual regressions, or QA-ing a change beyond unit tests.
---

# Driven agent QA runbook

The test stack has four layers - pick the CHEAPEST layer that can catch the
bug class you care about:

| Layer | What it proves | Entry point |
| --- | --- | --- |
| unit / engine | core logic | `cargo test --workspace` |
| chaos (engine-level faults) | engine survives hostile inputs | `just chaos`, `just chaos-fuzz` |
| app e2e (this runbook) | the REAL app works through UI+IPC | `just e2e` |
| visual regression | pixels did not rot | `pnpm -C ui run test:visual` |

## The containerized app e2e harness

The Dockerfile `e2e-runtime` stage packages the REAL desktop app (release
profile, embedded UI) with Xvfb + WebKitWebDriver + tauri-driver, a headless
gnome-keyring (so OS-keychain code paths are real), MinIO + toxiproxy +
iptables/tc for fault injection, an OpenSSH server for the SFTP backend, and
the `driven-e2e` suite binary.

- `just e2e` - build the image + run every scenario (exit 0 = pass/skip).
- `just e2e-run <names...>` - run specific scenarios (`driven-e2e list`).
- `just e2e-hold` - boot and HOLD a container for interactive exploration.
- CI: `.github/workflows/e2e.yml` runs on every release tag (gating
  release.yml's build) and via workflow_dispatch. NEVER wire it per-PR
  (owner decision, issue #239).

### Interactive exploration (the agent playground)

```sh
just e2e-hold
docker exec -it driven-e2e-hold bash
# inside the container:
driven-e2e doctor          # environment self-check
driven-e2e run-all         # or a single scenario
# raw WebDriver on 127.0.0.1:4444 (tauri-driver), e.g.:
curl -s -X POST http://127.0.0.1:4444/session -H 'Content-Type: application/json' \
  -d '{"capabilities":{"alwaysMatch":{"tauri:options":{"application":"/usr/local/bin/driven-app"}}}}'
```

From a session you can `execute/sync` arbitrary JS in the webview, including
`window.__TAURI_INTERNALS__.invoke('<command>', {...})` - the production IPC
surface - and `GET /session/<id>/screenshot` for visuals. The suite's
`crates/driven-e2e/src/session.rs` + `flows.rs` are the reference client.

### Isolation + fault seams (all added for this harness)

- `DRIVEN_DATA_DIR=<abs dir>` - relocates state.db + logs; any number of
  isolated app instances. (src-tauri/src/logging.rs `data_dir_override`)
- `DRIVEN_USE_FAKE_REMOTE=1` - in-memory fake Drive backend.
- `DRIVEN_TEST_FAULT_PLAN=<json path>` - arms faults on the fake remote of a
  RUNNING app (rate limits, 5xx, network drops, quota, dest-folder-gone...).
  Only honored with the fake remote; unknown fields fail loudly. Schema:
  `crates/driven-drive/src/fake/fault_plan.rs`.
- `DRIVEN_E2E_HOOKS=1` - enables `e2e_pick_folder`, the headless twin of the
  native folder-picker dialog (mints the same one-shot dialog token). This is
  the ONLY non-production IPC command; everything downstream is real.
- Wire faults for the S3 backend: toxiproxy sits between app and MinIO
  (see `crates/driven-e2e/src/scenarios/s3.rs`); `sudo iptables`/`tc` are
  available in-container for total-loss / shaping.
- Real network destinations are SIDECAR SUBPROCESSES on free localhost ports,
  not a compose stack: `S3Stack` (minio + toxiproxy) in `scenarios/s3.rs` and
  `SftpStack` (a real `sshd`) in `scenarios/sftp.rs` are the two templates to
  copy for a new backend. `SftpStack` runs sshd as the non-root `driven` user
  with a per-scenario ed25519 host key + client key minted by `ssh-keygen`
  into the scenario tempdir - no key material is baked into the image. That
  unprivileged server is also what makes the read-only-root probe real (root
  would bypass the mode bits); the flip side is that password auth cannot work
  there (it needs `/etc/shadow`), so the e2e tier uses key auth and password
  auth is covered by the driven-sftp unit tests + chaos `TestSftpServer`.
- Disk-full: run the container with `--tmpfs /e2e-small-dest:rw,size=1m`
  (the justfile recipes already do).

### Rollback / snapshots

Containers ARE the rollback: every scenario gets a fresh `DRIVEN_DATA_DIR`,
and a fresh `docker run` gets a pristine OS. For a mid-state checkpoint use
`docker commit driven-e2e-hold my-checkpoint` and start new containers from
that image.

## Headless CLI round trips (no GUI at all)

`driven-cli` shares the production engine. For state-DB inspection:
`driven-cli status|history|verify|scrub --db <state.db>`. For restores:
`driven-cli restore --db <state.db> --source-id <id> --dest <dir>
[--verify-against <dir>]` - downloads/decrypts through the production
fetch path and byte-compares. The `driven-core::restore_fetch` module is the
shared engine; chaos scenarios drive the same code under injected faults.

## Visual regression

- `pnpm -C ui run test:visual` - Playwright + committed LINUX baselines
  (CI-authoritative; macOS runs are advisory, darwin baselines gitignored).
- `just visual-update` - regenerate baselines via the official Playwright
  Docker image so they stay linux-rendered. Commit baseline changes ONLY when
  the visual change is intended.
- The scripted IPC mock (`ui/test-support/`) powers deterministic UI states
  (populated / empty / error) for both vitest and Playwright.
- AI screenshot review is a LOCAL/agent activity: `just e2e` scenarios save
  real-app screenshots under the artifacts dir - read them with vision and
  judge; CI only ever does deterministic pixel diffs.

## macOS local smoke (dev machine)

WKWebView has no WebDriver/CDP, so on macOS drive the app via a sandboxed
data dir + IPC-free assertions:

```sh
DRIVEN_DATA_DIR=$(mktemp -d) DRIVEN_USE_FAKE_REMOTE=1 DRIVEN_E2E_HOOKS=1 just dev-fake
# assert on $DRIVEN_DATA_DIR/state.db with driven-cli / sqlite3; screenshot
# the window with `screencapture -l` (osascript to focus) if needed
```

Prefer the Linux container for anything that needs DOM access or faults; use
the mac smoke only for platform-specific behavior (menu bar, APFS broker).

## Gotchas

- The e2e image binary MUST be a release build with the
  `driven-app/custom-protocol` feature, or the webview points at
  localhost:5173 and renders nothing (see Dockerfile comments).
- tauri-driver launches the app: per-scenario env goes on the tauri-driver
  process (the suite spawns one driver per scenario), not the container.
- The suite runs as the non-root `driven` user; permission-denial scenarios
  are meaningless as root. `sudo` is passwordless inside the e2e image.
- Two app instances cannot share one DRIVEN_DATA_DIR (state.db lock).
