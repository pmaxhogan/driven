# Driven - repo guidance for Claude

## Pull request titles MUST be Conventional Commits (load-bearing)

This repo **squash-merges**, and the squash commit subject is the **PR title**
(GitHub setting: "Pull request title and description"). `release-please` then
parses that subject as a [Conventional Commit](https://www.conventionalcommits.org/)
to build `CHANGELOG.md` and pick the next version. **A non-conventional PR
title is silently dropped from the changelog** - it does not fail the release,
it just vanishes from the notes (this is exactly what happened to #37, whose
title was `Comprehensive UI/UX overhaul ...`).

So whenever you open or rename a PR, the title MUST be:

```
type(optional-scope): short imperative summary
```

- **Allowed types:** `feat`, `fix`, `perf`, `refactor`, `docs`, `test`,
  `build`, `ci`, `chore`, `style`, `revert`.
- **Version impact (post-1.0):** `feat` -> minor (x.Y.0), `fix`/`perf` ->
  patch. A breaking change uses a `!` (`feat!:` / `fix(core)!:`) or a
  `BREAKING CHANGE:` footer in the PR body and bumps the **major** (X.0.0).
  (`bump-minor-pre-major` is still set in `release-please-config.json` but
  is inert now that the version is >= 1.0.0.)
- Changelog-visible by default: `feat` (Features), `fix` (Bug Fixes), `perf`,
  `revert`, and `deps` (Dependencies). The rest - `docs`, `chore`, `ci`,
  `build`, `refactor`, `style`, `test` - are valid but **hidden**: a PR titled
  with one of those passes the title gate yet produces no changelog entry and
  no version bump. So pick the *accurate* type - do NOT downgrade a real
  feature/fix to `chore:`/`docs:` just to clear the red X, or the change
  silently vanishes from the release notes (the same failure that hit #37,
  only self-inflicted).
- Scope is optional. Common scopes here: `core`, `cli`, `ui`, `updater`,
  `telemetry`, `ci`, `landing`, `capstone`.
- Subject after the colon may use any case (so `OAuth`, `CLI`, `macOS` are
  fine); just keep it short and imperative, no trailing period.

Good: `feat(ui): redesign the setup wizard`,
`fix(updater): floor the dev channel to stable`,
`ci: enforce conventional PR titles`.
Bad: `Comprehensive UI/UX overhaul`, `Update stuff`, `WIP`.

Individual commits on a feature branch do **not** need to be conventional -
they are squashed away (squash is the only merge method enabled on this repo),
so only the PR title reaches `main`. Don't waste effort rewriting branch commit
messages.

### Enforcement

`.github/workflows/pr-title.yml` (`amannn/action-semantic-pull-request`)
validates the title on every PR and is a **required status check** in the
`main protection` ruleset, so a bad title blocks the merge. If a title is
fixed after the fact, editing it re-runs the check automatically. The repo
owner has ruleset bypass, so this is block-by-default, overridable in a pinch -
prefer fixing the title over bypassing.

## Release flow (release-please)

- Every push to `main` updates the open `chore(main): release X.Y.Z` PR with
  the accumulated changelog + version bumps (`Cargo.toml` workspace version,
  `src-tauri/tauri.conf.json`, `ui/package.json` - see
  `release-please-config.json`).
- **Merging that release PR** creates the `vX.Y.Z` tag, which fires
  `release.yml` (the build/sign/publish pipeline). Don't tag by hand.
- If a change already landed on `main` with a non-conventional subject and is
  missing from the release PR, backfill it with an empty conventional commit:
  `git commit --allow-empty -m "feat: <restated summary> (#NN)"` then push.
  release-please will parse it and add the entry (it even linkifies `(#NN)`).
  A revert is not the fix - it would undo the change on `main`.

## Agent QA harness (test the app like a user, without a human)

Before hand-testing anything or asking the owner to QA, read the repo skill
`.claude/skills/driven-agent-qa/SKILL.md`. It documents the four-layer test
stack and the agent-facing entry points:

- `just e2e` - containerized app-level e2e: the REAL Linux desktop app driven
  over WebDriver (wizard, backup -> restore round trips, fault scenarios).
  Real destinations are sidecar subprocesses inside that image, not a compose
  stack: MinIO + toxiproxy (`crates/driven-e2e/src/scenarios/s3.rs`) and an
  OpenSSH server (`.../scenarios/sftp.rs`, key auth against an unprivileged
  sshd). Copy one of those two stacks when adding a backend.
- `just e2e-hold` - boot the container and drive the app interactively
  (WebDriver on :4444, IPC via `window.__TAURI_INTERNALS__.invoke`, faults
  via toxiproxy/iptables, screenshots for vision review).
- Seams: `DRIVEN_DATA_DIR` (isolated instances), `DRIVEN_TEST_FAULT_PLAN`
  (fault-inject a running app's fake remote), `DRIVEN_E2E_HOOKS=1`
  (headless dialog-token minting).
- `pnpm -C ui run test:visual` - Playwright visual regression against
  committed linux baselines (`just visual-update` regenerates via Docker).
- CI: `.github/workflows/e2e.yml` gates releases (tag -> e2e + visual ->
  build). Deliberately NOT per-PR; manual runs via workflow_dispatch.
