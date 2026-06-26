# Dev channel floor: dev never falls behind stable

Date: 2026-06-25
Status: Approved (design, revised after codex review)

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

For every updater target, at all times (after any whole-site updates deploy):

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
(`updates/<channel>/<os>/<arch>/update.json`), never in the body. The required
fields the updater consumes are `version`, `platforms.<key>.url`, and
`platforms.<key>.signature`; there is no channel field. The stable `url` points
at the permanent `/releases/download/v<version>/...` tag assets. Therefore
"make the dev channel serve the stable build" is literally **copying the stable
manifest JSON into the `dev/` path** - no rebuild, no re-signing, no base-URL
rewriting. Both channels validate against the **same** updater public key
(`tauri.conf.json`), so the copied signature verifies unchanged, and the stable
tag assets already exist when its manifests are generated (the release job
downloads them first).

## Approach: floor dev to stable at every whole-site deploy (no rebuild)

Three workflows publish a whole-site snapshot to the same `driven-updates`
Cloudflare Pages project, and any of them can leave dev below stable:

- `release.yml` (stable publish) overlays the live **dev** manifests, then
  deploys. After a version bump the overlaid dev can be **below** the new stable.
- `dev-channel.yml` (dev publish) overlays the live **stable** manifests, then
  deploys. A dev build whose checkout predates a stable release computes a
  **stale** dev version (from old `Cargo.toml`) that can be below the live
  stable - and the two workflows are in separate concurrency groups, so this
  race is real.
- `deploy-landing.yml` (landing publish) overlays **both** live channels, then
  deploys. If it fetched a stale dev (below stable) and deploys after a release,
  it **undoes** the floor.

So the floor is not a release-only concern. We enforce the invariant at the one
choke point all three share: **immediately before the whole-site
`pages deploy`, after the existing `fetch-live-channel.sh` overlay(s)**. Each of
the three workflows gets one added step:

```
node scripts/floor-dev-channel.mjs \
  --stable-dir site/updates/stable \
  --dev-dir    site/updates/dev \
  [--stable-version <known stable version>]   # release.yml passes it; the
                                              # others read it from the local
                                              # stable manifest tree
```

`dev-channel.yml` and `deploy-landing.yml` do not generate stable manifests, so
they rely on the **overlaid live stable** tree (`fetch-live-channel.sh stable`)
as the floor reference; `release.yml` has freshly generated stable manifests.
In all three, the floor compares per target and acts on the **local** tree about
to be deployed.

For each of the four GA targets (`windows/x86_64`, `darwin/x86_64`,
`darwin/aarch64`, `linux/x86_64`):

| Local dev manifest vs local stable | Action |
| --- | --- |
| dev version **<** stable | **overwrite** `dev/<plat>/update.json` with `stable/<plat>/update.json` (floor up) |
| dev version **>=** stable | **keep** the dev manifest (dev is already ahead - untouched) |
| dev **missing** (first-publish 404) | **seed** `dev/<plat>` from `stable/<plat>` |
| stable **missing** (no stable yet) | keep dev as-is; nothing to floor against |

### Local hard gate (primary), remote smoke (secondary)

After acting, `floor-dev-channel.mjs` **asserts** `dev >= stable` for every
target present in the local tree and exits non-zero if any target violates it.
This is the **hard correctness gate**: it runs before the deploy, is immune to
Cloudflare propagation lag, and aborts the deploy on a logic regression.

The existing `release.yml` post-deploy stable smoke is extended to also fetch
each deployed `dev/<plat>/update.json` and check `dev >= stable`, but this is a
**secondary** eventual-publication check using the existing bounded curl retry
(Pages propagation is non-atomic), not the primary gate - so a propagation lag
cannot false-fail the release while the local assertion already proved the tree
correct.

### Composition with the existing fail-closed overlay

`fetch-live-channel.sh` already fails closed: a *transient* (non-404) failure to
fetch a live manifest aborts the deploy. The floor step runs **after** the
overlay and only ever sees either a successfully-fetched manifest or a
definitive 404 (absent, handled by the seed/keep rows). It never relaxes the
fail-closed guarantee. The floor needs the real live versions to compare, so
this ordering is load-bearing.

## Components

### New file: `scripts/floor-dev-channel.mjs`

Pure-Node, no network, mirroring the tested-helper style of
`generate-update-json.mjs` and `set-dev-version.mjs`.

- `comparePrecedence(a, b) -> -1 | 0 | 1` - SemVer §11 precedence. Strips a
  leading `v`, compares `major.minor.patch` numerically, and ranks a clean
  release above a same-core prerelease (`0.2.0 > 0.2.0-dev.30`). The only
  comparison the floor and the smoke ever make is **clean-release stable** vs
  **prerelease dev** (or two clean releases), so "release outranks same-core
  prerelease" is the load-bearing rule; full identifier ordering is implemented
  defensively (numeric identifiers numerically, with leading-zero tolerance, so
  an all-digit short SHA cannot throw).
- `floorChannel({ stableDir, devDir, platforms, log }) -> { floored, kept,
  seeded, missingStable }` - the copy/keep/seed decision per target on local
  files only. Reads each `devDir/<plat>/update.json` and
  `stableDir/<plat>/update.json`, compares versions, copies the stable manifest
  verbatim into the dev path when stable is newer or dev is missing.
- `assertFloored({ stableDir, devDir, platforms })` - the hard gate: throws if
  any target has dev `<` stable after flooring.
- CLI entry parsing `--stable-dir`, `--dev-dir`, optional `--stable-version`
  (release path) and `--platforms`, wired into all three workflows.

### Changed: `.github/workflows/release.yml`

1. Add the `floor-dev-channel.mjs` step after `Overlay live dev manifests` and
   before the Cloudflare Pages deploy (passing the freshly-built
   `--stable-version`).
2. Extend the post-deploy stable smoke to also assert each deployed
   `dev/<plat>` version is `>=` stable (secondary check, existing retry).

### Changed: `.github/workflows/dev-channel.yml`

Add the floor step in the publish job after `Overlay live stable manifests` and
before the deploy. This floors a **stale in-flight dev build** up to live stable,
closing the cross-workflow race.

### Changed: `.github/workflows/deploy-landing.yml`

Add the floor step after the two overlay steps and before the deploy, so a
landing redeploy can never republish a below-stable dev manifest.

### Changed: `ui/src/views/About.vue`

The macOS manual-download link (`macDownloadUrl`) currently keys off
`updater.available.channel === "dev"` and always points a dev user at
`/releases/tag/dev`. After flooring, a macOS dev user can be offered a **stable**
build (clean version, assets on the stable tag), so the dev-tag link would send
them to the rolling dev release page that lacks those assets. Fix: derive the
link from the **offered version shape** - a prerelease (`-dev`) offer keeps the
`/releases/tag/dev` link; a clean-release offer (the floored case) links to that
release (`/releases/tag/v<version>`, falling back to `/releases/latest`). This
makes the floored update actually obtainable on macOS, where in-app install is
disabled.

## Testing

Vitest specs alongside the existing `generate-update-json` / `set-dev-version`
tests:

- `comparePrecedence`: release > same-core prerelease; numeric patch ordering;
  `dev.<n>` ordering; all-numeric SHA identifier does not throw; leading `v`
  stripped; equal versions return 0.
- `floorChannel`: stable-newer copies stable over dev; dev-newer keeps dev;
  dev-missing seeds; stable-missing keeps dev; a 4-target fixture mixes all
  cases in one run.
- `assertFloored`: passes when all dev >= stable; throws naming the offending
  target when one is below.
- `About.vue` `macDownloadUrl`: a `-dev` offer -> `/tag/dev`; a clean-release
  offer -> `/tag/v<version>`; no offer -> `/latest`.

## Out of scope (YAGNI)

- No change to `set-dev-version.mjs`'s version formula.
- No client/updater download-logic change beyond the macOS manual-link
  derivation above (the version comparison the updater already does is correct).
- No rebuild-on-release path (the rejected expensive alternative).
- No shared cross-workflow deploy lock: flooring at every deploy point makes the
  invariant self-healing regardless of deploy order, which is simpler and
  sufficient. (Noted as the alternative to a global deploy mutex.)

## Cost / risk

Near-zero runtime cost: one local-file step per deploy plus a secondary smoke
assertion; no extra build minutes. The local hard gate makes the invariant a
pre-deploy correctness check rather than relying on remote propagation. Blast
radius is confined to manifest assembly in the three deploy workflows plus one
computed property in the macOS About view.
