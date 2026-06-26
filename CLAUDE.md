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
- **Version impact (pre-1.0, `bump-minor-pre-major`):** `feat` -> minor
  (0.x.0), `fix`/`perf` -> patch. A breaking change uses a `!`
  (`feat!:` / `fix(core)!:`) or a `BREAKING CHANGE:` footer in the PR body.
- Only `feat`, `fix`, and `perf` show up in the changelog by default; the
  others are valid but hidden. Use the most accurate type regardless.
- Scope is optional. Common scopes here: `core`, `cli`, `ui`, `updater`,
  `telemetry`, `ci`, `landing`, `capstone`.
- Subject after the colon may use any case (so `OAuth`, `CLI`, `macOS` are
  fine); just keep it short and imperative, no trailing period.

Good: `feat(ui): redesign the setup wizard`,
`fix(updater): floor the dev channel to stable`,
`ci: enforce conventional PR titles`.
Bad: `Comprehensive UI/UX overhaul`, `Update stuff`, `WIP`.

Individual commits on a feature branch do **not** need to be conventional -
they are squashed away; only the PR title reaches `main`. Don't waste effort
rewriting branch commit messages.

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
