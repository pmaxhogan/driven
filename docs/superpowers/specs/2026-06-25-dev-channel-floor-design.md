# Dev channel floor: dev never falls behind stable

Date: 2026-06-25
Status: Approved (design)

## Problem

The rolling `dev` updater channel can fall **behind** the stable channel.

The dev version is computed by `scripts/set-dev-version.mjs` as
`<stable_patch+1>-dev.<run>.<sha>`, derived from the current
`[workspace.package].version`. A fresh dev build is therefore always greater
than the stable release it was cut from. But the dev channel is only rebuilt
when `dev-channel.yml` runs, and that workflow is deliberately gated (cost
policy): it builds only on `workflow_dispatch` or a `[dev-build]` commit, never
on an ordinary push to `main`, and never on a release-please release commit.

So when a stable release ships (`release.yml`, tag-triggered) without a
subsequent dev build, the live dev manifests keep advertising the **old** dev
version. Observed 2026-06-25: stable released `0.2.0` while the dev channel
stayed at `0.1.1-dev.24.ac5e487` - which is *lower* than stable. A user on the
dev channel is then stranded on a version older than stable, with no update
path until someone manually triggers a dev build.

## Invariant

For every updater target, after a stable release completes:

```
dev_manifest.version  >=  stable_manifest.version
```

"At a minimum" the dev channel must never serve a version below stable. It may
legitimately be *ahead* (a real dev build for the next version) - that case
must be preserved, not clobbered.

## Key enabler

A Tauri updater manifest body is **channel-agnostic**:

```json
{ "version": "...", "notes": "...", "pub_date": "...",
  "platforms": { "<os>-<arch>": { "signature": "...", "url": "..." } } }
```

The channel appears **only** in the directory path
(`updates/<channel>/<os>/<arch>/update.json`), never in the body. The stable
asset `url` points at the permanent `/releases/download/v<version>/...` tag
assets. Therefore "make the dev channel serve the stable build" is literally
**copying the stable manifest JSON into the `dev/` path** - no rebuild, no
re-signing, no base-URL rewriting. The same updater public key validates both
channels (one app, one key), so the copied signature verifies unchanged.

## Approach: floor dev to stable at release time (no rebuild)

The fix lives **entirely in `release.yml`**. `dev-channel.yml` is unchanged
(dev builds already self-correct above stable via `set-dev-version.mjs`).

`release.yml` already runs `scripts/fetch-live-channel.sh dev site/updates`
before the whole-site Cloudflare Pages deploy, which fetches the currently-live
dev manifests into the deploy tree so a stable deploy does not wipe the dev
channel. We add **one step immediately after that overlay**:

```
node scripts/floor-dev-channel.mjs \
  --stable-dir site/updates/stable \
  --dev-dir    site/updates/dev \
  --stable-version <freshly-built stable version>
```

For each of the four GA targets (`windows/x86_64`, `darwin/x86_64`,
`darwin/aarch64`, `linux/x86_64`):

| Overlaid live dev manifest | Action |
| --- | --- |
| version **<** stable | **overwrite** `dev/<plat>/update.json` with `stable/<plat>/update.json` (floor up) |
| version **>=** stable | **keep** the dev manifest (dev is already ahead - untouched) |
| **missing** (first-publish 404) | **seed** `dev/<plat>` from `stable/<plat>` (a brand-new dev channel starts at stable, never below) |

### Composition with the existing fail-closed policy

`fetch-live-channel.sh` already fails closed: a *transient* (non-404) failure to
fetch a live dev manifest aborts the deploy, because wiping a live channel is
worse than a skipped deploy. The floor step runs **after** that and only ever
sees either a successfully-fetched dev manifest or a definitive 404 (absent).
It never relaxes the fail-closed guarantee - a transient failure still aborts
before the floor runs. The floor needs the real live dev version to compare, so
this ordering is load-bearing.

## Components

### New file: `scripts/floor-dev-channel.mjs`

Pure-Node, no network, mirroring the tested-helper style of
`generate-update-json.mjs` and `set-dev-version.mjs`.

- `comparePrecedence(a, b) -> -1 | 0 | 1` - SemVer §11 precedence. Correctly
  ranks a clean release above a same-core prerelease (`0.2.0 > 0.2.0-dev.30`)
  and orders numeric prerelease identifiers (`dev.24 < dev.30`). No new
  dependency.
- `floorChannel({ stableDir, devDir, stableVersion, platforms, log }) ->
  { floored, kept, seeded }` - the copy/keep/seed decision per target, operating
  on local files only. Reads each `devDir/<plat>/update.json` (and the matching
  `stableDir/<plat>/update.json`), compares, and copies when stable is newer or
  dev is missing. Returns counts for logging.
- CLI entry parsing `--stable-dir`, `--dev-dir`, `--stable-version`, optional
  `--platforms`, wired into `release.yml`.

The stable manifest is the copy source (its body already carries the correct
stable `version`, `signature`, `url`, `notes`, `pub_date`); the dev path just
receives that file verbatim.

### Changed file: `.github/workflows/release.yml`

1. Add the `floor-dev-channel.mjs` step immediately after the
   `Overlay live dev manifests` step and before the Cloudflare Pages deploy.
2. Extend the existing post-deploy stable smoke step to also assert, per target,
   that the deployed `dev/<plat>/update.json` version is `>=` the stable
   version. This makes the invariant a hard release gate: a future regression of
   the floor fails the release instead of silently stranding dev users. The
   comparison reuses `comparePrecedence` (invoked via `node -e` or a tiny
   `--check` subcommand on the script) so the smoke and the floor share one
   precedence implementation.

## Testing

Vitest specs alongside the existing `generate-update-json` / `set-dev-version`
tests:

- `comparePrecedence`: release > same-core prerelease; numeric patch ordering;
  `dev.<n>` numeric identifier ordering; equal versions return 0; mixed
  major/minor/patch.
- `floorChannel`: stable-newer copies stable over dev; dev-newer keeps dev
  untouched; dev-missing seeds from stable; a full 4-target fixture exercises a
  mix (one ahead, one behind, one missing) in a single run.

## Out of scope (YAGNI)

- No change to `dev-channel.yml` or `set-dev-version.mjs`'s version formula.
- No client/updater (`src-tauri/src/updater.rs`) change - it would need an app
  release to propagate and would not help already-installed older clients.
- No rebuild-on-release path (the rejected expensive alternative).

## Cost / risk

Near-zero runtime cost: one local-file step plus a smoke assertion per release;
no extra build minutes. Blast radius is confined to release-time manifest
assembly, and the new post-deploy `dev >= stable` smoke gate catches a
regression before it reaches users.
