# `scripts/`

Release, deploy, and coverage helpers. Almost all of them are invoked by a workflow
in `.github/workflows/`, not by hand - each file's header comment explains the
failure it exists to prevent.

- `generate-update-json.mjs` - writes the per-target Tauri updater manifests
  (`updates/<channel>/<os>/<arch>/update.json`) the in-app updater fetches
- `extract-changelog.mjs` - pulls one version's section out of `CHANGELOG.md` so the
  GitHub Release body and the in-app changelog get the same real notes
- `set-dev-version.mjs` - patches the dev-channel version into every canonical
  version source before a dev build
- `floor-dev-channel.mjs` - keeps the dev channel from ever advertising a version
  below stable
- `fetch-live-channel.sh` - overlays the *other* channel's live manifests before a
  Cloudflare Pages deploy, which publishes a whole-site snapshot and would otherwise
  wipe them
- `assemble-landing.sh` - copies `site-landing/` into the same deploy staging root
- `coverage.sh` - reproduces the `coverage` CI gate's Rust + UI percentages locally
  (needs `cargo-llvm-cov` and `jq`; run under git-bash or WSL on Windows)

`just coverage` is the shorthand for the last one.
