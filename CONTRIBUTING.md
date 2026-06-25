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

## Branch and PR flow

1. Branch off `main`.
2. Make your change with Conventional-Commit messages.
3. Run the local gates above until green.
4. Open a pull request against `main`. CI runs the same gates plus the chaos
   harness.
5. Keep the PR focused; smaller PRs review faster.

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
