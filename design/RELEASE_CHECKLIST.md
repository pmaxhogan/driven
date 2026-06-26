# Driven release checklist

Per-release manual steps for cutting a tagged GA release (referenced by ROADMAP).
The pipeline is mostly automated (release-please -> `v*` tag -> `release.yml`); this
checklist covers the human gates + the post-deploy verification that CI cannot fully
self-assert. ASCII only.

## How a release flows

1. Conventional-commit work lands on `main`. `release-please.yml` maintains a
   "chore(main): release X.Y.Z" PR (version bump + CHANGELOG). It authors the PR via
   `RELEASE_PLEASE_TOKEN` (a real-identity token) so the PR's CI runs without the
   bot-PR "approve workflows" gate.
2. Merging that PR creates the `vX.Y.Z` git tag.
3. The tag fires `release.yml`: build + sign (ed25519) 4 targets, attach to the GitHub
   Release, generate `updates/stable/<os>/<arch>/update.json`, overlay the live dev
   channel, copy the landing page, deploy the whole `site/` to Cloudflare Pages
   (`driven-updates` -> driven.maxhogan.dev), then smoke-test the deployed manifests.

## Pre-release

- [ ] `main` CI is green (all 12 required checks) at the commit you're releasing.
- [ ] CHANGELOG entry for this version reads correctly (release-please generates it;
      fix wording in the release PR before merging if needed).
- [ ] Version bump is the intended semver (feat -> minor, fix -> patch). A feature
      commit since the last tag means a MINOR bump - confirm that's intended.
- [ ] No `-dev.*` / non-numeric pre-release in the stable version (the Windows MSI/WiX
      bundler rejects non-numeric pre-releases; stable must be numeric).

## Cut the release

- [ ] Merge the release-please PR (this is the deliberate human gate). Confirm the
      `vX.Y.Z` tag was created and `release.yml` started.
- [ ] Watch `release.yml` to green: 4 build jobs + `publish stable update manifests`.

## Post-deploy verification (the part CI can't fully self-check)

- [ ] All 4 stable manifests serve the NEW version at the app's real fetch path
      (custom domain), e.g.:
      `curl -s https://driven.maxhogan.dev/updates/stable/<os>/<arch>/update.json`
      for windows/x86_64, darwin/x86_64, darwin/aarch64, linux/x86_64 -> HTTP 200 +
      `"version":"X.Y.Z"`. The release smoke does this, but re-check after any
      concurrent deploy (landing/dev-channel) that may have raced.
- [ ] CACHE GOTCHA: `driven.maxhogan.dev/updates/*` MUST NOT be edge-cached. A zone
      cache rule "Bypass cache" for `http.host eq "driven.maxhogan.dev" and
      starts_with(http.request.uri.path, "/updates/")` is in place; confirm responses
      show `Cf-Cache-Status: DYNAMIC` (not HIT). If a stale version/404 is served from
      the custom domain while `https://driven-updates.pages.dev/updates/...` is correct,
      the cache rule is missing/broken - re-add it and purge `/updates/*`. (A long-cached
      manifest silently breaks auto-update for everyone and can cache a transient 404.)
- [ ] GitHub Release has all expected assets + `.sig` files for all 4 targets
      (msi+nsis for Windows stable, dmg/app.tar.gz per mac arch, AppImage/deb for Linux).
- [ ] Landing root `https://driven.maxhogan.dev/` serves 200 (whole-site deploy didn't
      wipe it).

## Updater smoke (do at least once per release line)

- [ ] Install the PREVIOUS published version, launch it, and confirm it auto-updates to
      this version on Windows + Linux (detect -> download -> verify sig -> apply ->
      restart). macOS is the documented manual-DMG path (unsigned binaries) - skip
      in-app update there.

## Telemetry (once the worker is deployed)

- [ ] `POST https://driven.maxhogan.dev/telemetry/v1/ping` (empty body) -> the worker's
      expected response (e.g. 400 for bad body, 405 for GET), NOT a CF Pages 404/405.
      Confirm a row lands in the `driven_telemetry` Analytics Engine dataset.

## After release

- [ ] Smoke the in-app "Check for updates" / changelog modal shows this version's notes.
- [ ] Update any "Shipped in X.Y.Z" doc sections (DESIGN/ROADMAP) for newly-GA features.
