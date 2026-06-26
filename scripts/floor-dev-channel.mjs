#!/usr/bin/env node
// floor-dev-channel.mjs
//
// Guarantee the rolling `dev` updater channel never advertises a version BELOW
// the stable channel (design:
// docs/superpowers/specs/2026-06-25-dev-channel-floor-design.md).
//
// WHY. The dev version (`set-dev-version.mjs`) is `<stable_patch+1>-dev.<run>.<sha>`,
// always ABOVE the stable it was cut from. But the dev channel is only rebuilt on
// an explicit dev build, so a stable release that ships WITHOUT a following dev
// build leaves the live dev manifests advertising an OLD (now below-stable)
// version - stranding dev-channel users on something older than stable.
//
// HOW. A Tauri updater manifest body is channel-agnostic: the channel lives ONLY
// in the directory path (`updates/<channel>/<os>/<arch>/update.json`), never in
// the JSON. Both channels validate against the same updater pubkey, and the
// stable `url` points at the permanent `/releases/download/v<ver>` tag assets.
// So "make dev serve the stable build" is literally COPYING the stable manifest
// into the `dev/` path. This script does that per target whenever stable
// outranks the local dev manifest (and seeds dev when it is absent), then asserts
// the floor held. It runs - on the LOCAL tree about to be deployed - before every
// whole-site `pages deploy` of the `driven-updates` site (release.yml,
// dev-channel.yml, deploy-landing.yml), so the invariant is self-healing
// regardless of which workflow deploys last.
//
// Usage:
//   node scripts/floor-dev-channel.mjs --stable-dir <dir> --dev-dir <dir> \
//       [--platforms "windows/x86_64,darwin/x86_64,..."]
//       # floor dev to stable across the tree, then assert dev >= stable.
//   node scripts/floor-dev-channel.mjs --ge <a> <b>
//       # exit 0 if SemVer-precedence(a) >= precedence(b), else exit 1 (used by
//       # the release post-deploy smoke as a secondary dev>=stable check).
//
// PURE Node, NO network: reads/copies local `update.json` files only.

import { promises as fs } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);

/** The GA platform matrix as `<os>/<arch>` path segments, matching the updater
 * endpoint layout `updates/<channel>/<os>/<arch>/update.json`. Keep in sync with
 * `fetch-live-channel.sh` PLATFORMS and the build matrices. */
export const V1_PLATFORMS = [
  "windows/x86_64",
  "darwin/x86_64",
  "darwin/aarch64",
  "linux/x86_64",
];

/** Parse a SemVer-ish string (tolerating a leading `v`) into its core + ordered
 * prerelease identifiers. Throws on an unparseable shape. Build metadata
 * (`+...`) is ignored for precedence (SemVer §10). */
export function parseSemver(v) {
  const s = String(v).trim().replace(/^v/, "");
  const m = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(s);
  if (!m) {
    throw new Error(`unparseable semver: ${JSON.stringify(v)}`);
  }
  return {
    major: Number(m[1]),
    minor: Number(m[2]),
    patch: Number(m[3]),
    prerelease: m[4] ? m[4].split(".") : [],
  };
}

const isNumericId = (id) => /^[0-9]+$/.test(id);

/** SemVer §11 precedence: -1 if a < b, 0 if equal, 1 if a > b.
 *
 * The only comparison the floor + smoke ever make is a CLEAN-RELEASE stable
 * against a PRERELEASE dev (or two clean releases), so the load-bearing rule is
 * §11.3 "a clean release outranks a same-core prerelease". Full identifier
 * ordering (§11.4) is implemented defensively so the function is correct for any
 * input and an all-digit short SHA prerelease identifier compares numerically
 * (with leading-zero tolerance) rather than throwing. */
export function comparePrecedence(a, b) {
  const pa = parseSemver(a);
  const pb = parseSemver(b);
  for (const k of ["major", "minor", "patch"]) {
    if (pa[k] !== pb[k]) return pa[k] < pb[k] ? -1 : 1;
  }
  // Equal core. §11.3: a version WITHOUT a prerelease has higher precedence.
  const aClean = pa.prerelease.length === 0;
  const bClean = pb.prerelease.length === 0;
  if (aClean && bClean) return 0;
  if (aClean) return 1;
  if (bClean) return -1;
  // §11.4: both prerelease - compare identifiers left to right.
  const len = Math.max(pa.prerelease.length, pb.prerelease.length);
  for (let i = 0; i < len; i++) {
    const ia = pa.prerelease[i];
    const ib = pb.prerelease[i];
    if (ia === undefined) return -1; // §11.4.4: the shorter set is lower
    if (ib === undefined) return 1;
    if (ia === ib) continue;
    const na = isNumericId(ia);
    const nb = isNumericId(ib);
    if (na && nb) {
      const da = Number(ia);
      const db = Number(ib);
      if (da !== db) return da < db ? -1 : 1;
    } else if (na !== nb) {
      return na ? -1 : 1; // §11.4.3: numeric identifiers are lower than alphanumeric
    } else {
      return ia < ib ? -1 : 1; // §11.4.2: ASCII ordering
    }
  }
  return 0;
}

async function pathExists(file) {
  try {
    await fs.stat(file);
    return true;
  } catch {
    return false;
  }
}

/** Read and validate the `version` string out of an `update.json` file. */
async function readManifestVersion(file) {
  const raw = await fs.readFile(file, "utf8");
  const obj = JSON.parse(raw);
  if (typeof obj.version !== "string" || obj.version.length === 0) {
    throw new Error(`manifest ${file} has no version string`);
  }
  return obj.version;
}

/** Assert a parsed manifest carries the fields the updater needs - a non-empty
 * `platforms` map whose every entry has a non-empty `signature` + `url`. Guards
 * the copy paths so a malformed STABLE manifest (e.g. a corrupt live fetch) is
 * never propagated into the dev channel; a manifest carrying only a valid
 * `version` would otherwise pass the version check and be copied blindly. */
function assertServeable(obj, file) {
  const plats = obj && obj.platforms;
  if (!plats || typeof plats !== "object" || Object.keys(plats).length === 0) {
    throw new Error(`manifest ${file} has no platforms`);
  }
  for (const [key, entry] of Object.entries(plats)) {
    if (!entry || typeof entry.url !== "string" || entry.url.length === 0) {
      throw new Error(`manifest ${file} platform ${key} has no url`);
    }
    if (typeof entry.signature !== "string" || entry.signature.length === 0) {
      throw new Error(`manifest ${file} platform ${key} has no signature`);
    }
  }
}

/** Read a STABLE manifest about to be copied into the dev channel, validating
 * both its version AND its serveable shape. Returns its version. */
async function readServeableStableVersion(file) {
  const raw = await fs.readFile(file, "utf8");
  const obj = JSON.parse(raw);
  if (typeof obj.version !== "string" || obj.version.length === 0) {
    throw new Error(`manifest ${file} has no version string`);
  }
  assertServeable(obj, file);
  return obj.version;
}

/** Floor the dev channel to stable across `platforms`, operating on LOCAL files.
 *
 * Per target:
 *  - stable missing  -> leave dev untouched (stable never published this platform).
 *  - dev missing      -> SEED dev from stable (first-publish: dev starts at stable).
 *  - dev < stable     -> FLOOR: copy the stable manifest verbatim into the dev path.
 *  - dev >= stable    -> KEEP dev (a real dev build that is already ahead).
 *
 * The stable manifest is copied byte-for-byte because the body is channel-agnostic
 * (its `url` already points at the permanent stable tag assets). Returns counts. */
export async function floorChannel({ stableDir, devDir, platforms = V1_PLATFORMS, log = console }) {
  let floored = 0;
  let kept = 0;
  let seeded = 0;
  let missingStable = 0;
  for (const plat of platforms) {
    const stableFile = path.join(stableDir, plat, "update.json");
    const devFile = path.join(devDir, plat, "update.json");
    if (!(await pathExists(stableFile))) {
      missingStable++;
      log.warn?.(`no stable manifest for ${plat}; leaving dev as-is`);
      continue;
    }
    // Validate the stable manifest's serveable shape BEFORE any copy below, so a
    // malformed stable manifest is never seeded/floored into the dev channel.
    const stableVersion = await readServeableStableVersion(stableFile);
    if (!(await pathExists(devFile))) {
      await fs.mkdir(path.dirname(devFile), { recursive: true });
      await fs.copyFile(stableFile, devFile);
      seeded++;
      log.info?.(`seeded dev/${plat} from stable (${stableVersion})`);
      continue;
    }
    const devVersion = await readManifestVersion(devFile);
    if (comparePrecedence(stableVersion, devVersion) > 0) {
      await fs.copyFile(stableFile, devFile);
      floored++;
      log.info?.(`floored dev/${plat}: ${devVersion} -> ${stableVersion}`);
    } else {
      kept++;
      log.info?.(`kept dev/${plat} at ${devVersion} (>= stable ${stableVersion})`);
    }
  }
  return { floored, kept, seeded, missingStable };
}

/** HARD GATE: throw if, after flooring, any target has dev < stable (or a stable
 * manifest exists with no dev manifest). Runs on the LOCAL tree before deploy, so
 * it is immune to Cloudflare propagation lag - the authoritative correctness
 * check. */
export async function assertFloored({ stableDir, devDir, platforms = V1_PLATFORMS }) {
  const violations = [];
  for (const plat of platforms) {
    const stableFile = path.join(stableDir, plat, "update.json");
    const devFile = path.join(devDir, plat, "update.json");
    if (!(await pathExists(stableFile))) continue; // nothing to floor against
    const stableVersion = await readManifestVersion(stableFile);
    if (!(await pathExists(devFile))) {
      violations.push(`${plat}: dev manifest missing while stable=${stableVersion}`);
      continue;
    }
    const devVersion = await readManifestVersion(devFile);
    if (comparePrecedence(devVersion, stableVersion) < 0) {
      violations.push(`${plat}: dev=${devVersion} < stable=${stableVersion}`);
    }
  }
  if (violations.length > 0) {
    throw new Error(`dev channel is below stable after floor:\n  ${violations.join("\n  ")}`);
  }
}

const USAGE =
  "usage:\n" +
  "  node scripts/floor-dev-channel.mjs --stable-dir <dir> --dev-dir <dir> [--platforms <list>]\n" +
  "  node scripts/floor-dev-channel.mjs --ge <a> <b>\n";

function parseArgs(argv) {
  const opts = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const take = () => {
      const v = argv[i + 1];
      if (v === undefined) throw new Error(`missing value for ${a}`);
      i++;
      return v;
    };
    switch (a) {
      case "--stable-dir":
        opts.stableDir = take();
        break;
      case "--dev-dir":
        opts.devDir = take();
        break;
      case "--platforms":
        opts.platforms = take()
          .split(/[,\s]+/)
          .map((s) => s.trim())
          .filter((s) => s.length > 0);
        break;
      case "--ge":
        opts.geA = take();
        opts.geB = take();
        break;
      case "--help":
      case "-h":
        opts.help = true;
        break;
      default:
        throw new Error(`unknown option: ${a}`);
    }
  }
  return opts;
}

async function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (e) {
    process.stderr.write(`error: ${e.message}\n\n${USAGE}`);
    process.exit(2);
  }
  if (opts.help) {
    process.stdout.write(USAGE);
    process.exit(0);
  }
  // Compare mode: exit 0 if a >= b by SemVer precedence, else 1.
  if (opts.geA !== undefined) {
    try {
      process.exit(comparePrecedence(opts.geA, opts.geB) >= 0 ? 0 : 1);
    } catch (e) {
      process.stderr.write(`error: ${e.message}\n`);
      process.exit(2);
    }
  }
  // Floor mode.
  if (!opts.stableDir || !opts.devDir) {
    process.stderr.write(`error: --stable-dir and --dev-dir are required\n\n${USAGE}`);
    process.exit(2);
  }
  const platforms = opts.platforms ?? V1_PLATFORMS;
  try {
    const counts = await floorChannel({
      stableDir: opts.stableDir,
      devDir: opts.devDir,
      platforms,
    });
    await assertFloored({ stableDir: opts.stableDir, devDir: opts.devDir, platforms });
    process.stdout.write(
      `floor-dev-channel: floored=${counts.floored} kept=${counts.kept} ` +
        `seeded=${counts.seeded} missingStable=${counts.missingStable}; dev >= stable verified\n`,
    );
  } catch (e) {
    process.stderr.write(`error: ${e.message}\n`);
    process.exit(1);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === __filename) {
  main();
}
