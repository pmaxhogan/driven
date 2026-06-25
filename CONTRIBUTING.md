# Contributing to Driven

Thanks for your interest in improving Driven. This guide covers how to set up
the project, the gates your change must pass, the commit-message format, and the
house rules.

By participating you agree to abide by the [Code of Conduct](CODE_OF_CONDUCT.md).

## Prerequisites

- Rust stable (`rustup install stable`)
- Node.js 22+ and pnpm 10+
- `cargo install tauri-cli@^2 cargo-deny cargo-watch just`
  (Windows users can install `just` via `scoop install just`)
- Linux build deps: `libwebkit2gtk-4.1-dev libxdo-dev libssl-dev`
  `libayatana-appindicator3-dev librsvg2-dev libsoup-3.0-dev`
  `javascriptcoregtk-4.1`

Install the UI dependencies once:

```sh
pnpm --dir ui install
```

## Conventional Commits (required)

Driven uses [Conventional Commits](https://www.conventionalcommits.org/) because
releases and the changelog are automated with
[release-please](https://github.com/googleapis/release-please). The commit
subject drives the next version bump and the generated `CHANGELOG.md` entry, so
the format is not optional.

Format:

```
<type>(<optional scope>): <short summary>

<optional body>

<optional footer>
```

Common types:

- `feat`: a new user-facing feature (bumps the minor version pre-1.0)
- `fix`: a bug fix (bumps the patch version)
- `docs`: documentation only
- `refactor`: code change that neither fixes a bug nor adds a feature
- `test`: adding or fixing tests
- `chore`: tooling, deps, or maintenance
- `ci`: CI / workflow changes
- `perf`: a performance improvement

Breaking changes: add a `!` after the type/scope (for example `feat!:`) and / or
a `BREAKING CHANGE:` footer. Pre-1.0, a breaking change bumps the minor version.

Examples:

```
feat(restore): add full-text search over remote file names
fix(executor): retry resumable upload after a transient 5xx
docs: document the macOS auto-updater caveat in the README
```

## Local gates (run before opening a PR)

All cargo commands run with `SQLX_OFFLINE=true` because the SQL is checked
against the committed `.sqlx/` offline cache. The quickest path is the `just`
recipes:

```sh
just lint   # cargo fmt --all -- --check ; cargo clippy --workspace --all-targets -- -D warnings ; pnpm --dir ui lint
just test   # cargo test --workspace ; pnpm --dir ui test:unit
just deny   # cargo deny check
just coverage  # Rust + UI line coverage (must not regress vs main; see below)
```

Run them individually if you prefer:

```sh
# Rust
SQLX_OFFLINE=true cargo build --workspace --all-targets
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
SQLX_OFFLINE=true cargo test --workspace
cargo fmt --all -- --check
cargo deny check
git diff --check          # catches trailing whitespace and conflict markers

# UI (from the repo root)
pnpm --dir ui install
pnpm --dir ui lint
pnpm --dir ui exec prettier --check src
pnpm --dir ui build       # vue-tsc --noEmit must be clean
```

If you change a `sqlx::query!` / `query_as!`, regenerate the offline cache with
`just sqlx-prepare` (needs `cargo install sqlx-cli`) and commit the updated
`.sqlx/` directory.

If you change a GitHub Actions workflow under `.github/workflows/`, run
`actionlint` on it.

Some tests gate-skip honestly when the host cannot satisfy a requirement (for
example real-Google-Drive end-to-end tests with no credentials, or VSS /
elevation tests without admin). A clean run is all-pass plus those honest skips,
not a hidden failure.

## Coverage gate

The `coverage` workflow (`.github/workflows/coverage.yml`) measures line
coverage on every PR and **fails the check if either number regresses below
`main`** (minus a 0.1pp epsilon that absorbs float jitter). It posts a sticky
PR comment with the `main` baseline, this PR's number, and the delta. New code
must come with tests that keep coverage from dropping.

Two numbers are gated:

- **Rust** - the library crates (`cargo llvm-cov --workspace --exclude
  src-tauri --exclude driven-chaos`). `src-tauri` is a thin IPC layer over
  `driven-core` and `driven-chaos` is the stress harness, so neither is part of
  the gate; put real logic in `driven-core` (where it is unit-tested) and keep
  `src-tauri` commands thin.
- **UI** - the whole Vue/TS app (`ui/`, vitest v8 coverage with `all: true`, so
  a new untested file lowers the total).

How the baseline works: every push to `main` recomputes coverage and caches it;
PRs compare against that cached number. The first `main` build that carries the
workflow has no prior baseline, so that one run is informational, then the gate
enforces from the next PR onward.

Run it locally before pushing:

```sh
just coverage          # prints Rust + UI line-coverage totals
./scripts/coverage.sh  # same, parsed to the exact percentages CI compares
```

`just coverage` needs `cargo install cargo-llvm-cov`; the parsed script also
needs `jq`.

> Maintainer setup (one-time): mark **coverage** as a Required status check in
> the `main` branch protection rule so a regression actually blocks merge. The
> workflow failing is necessary but not sufficient until the check is required.

## Branch and PR flow

1. Branch off `main`, one branch per logical change. Name it `<type>/<slug>`
   (for example `feat/schedule-windows`).
2. Make your change with Conventional-Commit messages.
3. Run the local gates above until green.
4. Open a pull request against `main`. CI runs the same gates plus the chaos
   harness and the coverage gate.
5. Keep the PR focused; smaller PRs review faster.

### Squash merge (required)

PRs land on `main` via **Squash and merge** - the whole branch becomes one
commit. Because that squash commit's subject is what release-please reads, set
the squash subject to a Conventional-Commit line that summarises the PR (GitHub
pre-fills it from the PR title, so title your PR in Conventional-Commit form,
for example `feat(restore): point-in-time restore`). Individual
work-in-progress commit messages on the branch do not need to be release-grade;
only the squash subject does. This keeps `main` linear and one-commit-per-PR.

### Stacking dependent PRs

If a change depends on another that is still in review, branch it off the
dependency's branch instead of `main` and say so in the PR description ("stacked
on #NN"). Its diff will show the parent's changes until the parent merges;
rebase onto `main` after the parent lands. Independent changes should branch off
`main` so each PR diff is self-contained.

Releases are cut by release-please: merging the maintained "chore: release" PR
tags `v*` and triggers the build / publish pipeline. Do not hand-edit version
numbers or `CHANGELOG.md` release sections; release-please owns those.

## House rules

These keep the codebase clean across Windows, macOS, and Linux:

- ASCII only in source, docs, logs, and commit messages. Do NOT use em-dashes or
  en-dashes; use the ASCII hyphen-minus (`-`). Non-ASCII dashes render as
  garbage in Windows terminals and some viewers.
- LF line endings only. The repo's `.gitattributes` enforces this; do not commit
  CRLF.
- Keep `driven-core` free of direct I/O: it holds the traits and pure logic;
  side-effecting implementations live in their own crates.
- Never log file names, paths, or content from encrypted sources, and never put
  user data in telemetry.
- Match existing patterns and the design docs under `design/` rather than
  introducing parallel approaches.

Thanks for contributing.
