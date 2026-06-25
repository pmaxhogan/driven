#!/usr/bin/env node
// set-dev-version.mjs (ROADMAP M9 R1-P1-5, R2-P1-1).
//
// Patch the app version into ALL canonical version sources BEFORE a dev-channel
// Tauri build, so the produced app actually reports the dev version. Without this
// the dev build still reports the stable version, so the updater's version
// comparison (running vs manifest) is wrong and a dev user would never see a
// newer dev build (or would see a spurious downgrade).
//
// R2-P1-1: the dev version MUST be (a) greater than the current stable release
// and (b) monotonic between dev builds. The old `0.0.0-dev.<sha>` form was LOWER
// than stable `0.1.0` (so a stable user opting into `dev` was NEVER offered an
// update) and short SHAs do not sort by time (non-monotonic). The new form is
//   <next-patch>-dev.<run_number>.<short-sha>
// derived from the CURRENT [workspace.package].version (NOT hardcoded, so it
// tracks future stable bumps): e.g. stable 0.1.0 -> 0.1.1-dev.123.abc1234.
// SemVer ordering: 0.1.1-dev.* > 0.1.0 (0.1.1 > 0.1.0 numerically, regardless of
// the prerelease tag), and run_number being a numeric prerelease identifier makes
// successive dev builds strictly increasing (124 > 123). The COMPUTED value is
// used byte-identically for the app metadata patch AND the generated manifest
// (the workflow computes it once via `--print-dev-version` and threads it into
// both `set-dev-version.mjs <version>` and `generate-update-json.mjs --version`).
//
// The three sources release-please bumps in lockstep (release-please-config.json)
// are the same three we patch here, so the dev build mirrors a real release:
//   1. root Cargo.toml  [workspace.package].version  (src-tauri uses
//      version.workspace = true, so this is THE Rust/app version)
//   2. src-tauri/tauri.conf.json  .version
//   3. ui/package.json  .version
//
// Usage:
//   node scripts/set-dev-version.mjs <new-version>          # patch the 3 sources
//   node scripts/set-dev-version.mjs --print-dev-version <run_number> <sha>
//       # compute + print <next-patch>-dev.<run_number>.<sha> from the current
//       # workspace version (does NOT write anything) - the workflow captures
//       # this and feeds the SAME value to the patch step + the manifest.

import { promises as fs } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..");

/** Validate a SemVer-ish version (incl. the <x>-dev.<run>.<sha> prerelease form). */
export function isValidVersion(v) {
  return /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(v);
}

/** Read the `[workspace.package]` version string out of a Cargo.toml string.
 * Pure (string in -> string out), mirroring `setWorkspaceVersion`'s table walk
 * so the two stay consistent. Throws if the table / version is absent. */
export function readWorkspaceVersion(toml) {
  const lines = toml.split("\n");
  let inWorkspacePackage = false;
  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      inWorkspacePackage = trimmed === "[workspace.package]";
      continue;
    }
    if (inWorkspacePackage && /^version\s*=/.test(trimmed)) {
      const m = trimmed.match(/^version\s*=\s*"([^"]+)"/);
      if (m) return m[1];
    }
  }
  throw new Error("could not find [workspace.package] version in Cargo.toml");
}

/** R2-P1-1: derive the monotonic, above-stable dev version from the current
 * stable `version` (its PATCH bumped) plus the CI `runNumber` + short `sha`:
 *   `<major>.<minor>.<patch+1>-dev.<runNumber>.<sha>`
 * Pure + unit-testable. Requires a clean release `<x>.<y>.<z>` base (a dev base
 * would already carry a prerelease tag - we never compound those). `runNumber`
 * is the strictly-increasing GitHub `github.run_number` (a numeric SemVer
 * prerelease identifier, so successive builds sort by it); `sha` disambiguates +
 * traces the commit but does NOT drive ordering. */
export function computeDevVersion(currentVersion, runNumber, sha) {
  const m = /^(\d+)\.(\d+)\.(\d+)$/.exec(currentVersion);
  if (!m) {
    throw new Error(
      `current version must be a clean <major>.<minor>.<patch> release to derive a dev version (got \`${currentVersion}\`)`,
    );
  }
  const run = String(runNumber).trim();
  if (!/^\d+$/.test(run)) {
    throw new Error(`run_number must be a non-negative integer (got \`${runNumber}\`)`);
  }
  const shortSha = String(sha).trim();
  if (!/^[0-9A-Za-z]+$/.test(shortSha)) {
    throw new Error(`sha must be alphanumeric (got \`${sha}\`)`);
  }
  const [, major, minor, patch] = m;
  const nextPatch = Number(patch) + 1;
  return `${major}.${minor}.${nextPatch}-dev.${run}.${shortSha}`;
}

/** Compute the dev version from the repo's CURRENT Cargo.toml workspace version
 * (the `--print-dev-version` CLI path). Reads the real file; pure logic lives in
 * `computeDevVersion` / `readWorkspaceVersion`. */
export async function computeDevVersionFromRepo(runNumber, sha) {
  const raw = await fs.readFile(path.join(REPO_ROOT, "Cargo.toml"), "utf8");
  return computeDevVersion(readWorkspaceVersion(raw), runNumber, sha);
}

/** Replace the FIRST `version = "..."` line that appears within the
 * `[workspace.package]` table of a Cargo.toml string. Pure (string in/out) so
 * it is unit-testable; only touches the workspace.package version, never a
 * dependency `version = ` line. */
export function setWorkspaceVersion(toml, version) {
  const lines = toml.split("\n");
  let inWorkspacePackage = false;
  let replaced = false;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const trimmed = line.trim();
    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      inWorkspacePackage = trimmed === "[workspace.package]";
      continue;
    }
    if (inWorkspacePackage && /^version\s*=/.test(trimmed)) {
      lines[i] = `version = "${version}"`;
      replaced = true;
      break;
    }
  }
  if (!replaced) {
    throw new Error("could not find [workspace.package] version in Cargo.toml");
  }
  return lines.join("\n");
}

/** Set the top-level `version` string in a JSON file's parsed object. */
export function setJsonVersion(obj, version) {
  return { ...obj, version };
}

async function patchCargo(version) {
  const p = path.join(REPO_ROOT, "Cargo.toml");
  const raw = await fs.readFile(p, "utf8");
  await fs.writeFile(p, setWorkspaceVersion(raw, version), "utf8");
  return p;
}

async function patchJson(relPath, version) {
  const p = path.join(REPO_ROOT, relPath);
  const raw = await fs.readFile(p, "utf8");
  const obj = JSON.parse(raw);
  const next = setJsonVersion(obj, version);
  await fs.writeFile(p, JSON.stringify(next, null, 2) + "\n", "utf8");
  return p;
}

export async function setDevVersion(version) {
  if (!isValidVersion(version)) {
    throw new Error(`invalid version: ${version}`);
  }
  const patched = [];
  patched.push(await patchCargo(version));
  patched.push(await patchJson(path.join("src-tauri", "tauri.conf.json"), version));
  patched.push(await patchJson(path.join("ui", "package.json"), version));
  return patched;
}

const USAGE =
  "usage:\n" +
  "  node scripts/set-dev-version.mjs <new-version>\n" +
  "  node scripts/set-dev-version.mjs --print-dev-version <run_number> <sha>\n";

async function main() {
  const arg = process.argv[2];
  if (!arg) {
    process.stderr.write(USAGE);
    process.exit(2);
  }
  // R2-P1-1: compute + print the dev version (no writes) so the workflow derives
  // it ONCE and threads the SAME byte-identical value to the patch step + the
  // manifest generator.
  if (arg === "--print-dev-version") {
    const runNumber = process.argv[3];
    const sha = process.argv[4];
    if (!runNumber || !sha) {
      process.stderr.write(USAGE);
      process.exit(2);
    }
    try {
      const version = await computeDevVersionFromRepo(runNumber, sha);
      process.stdout.write(`${version}\n`);
    } catch (e) {
      process.stderr.write(`error: ${e.message}\n`);
      process.exit(1);
    }
    return;
  }
  const version = arg;
  try {
    const patched = await setDevVersion(version);
    for (const p of patched) {
      process.stdout.write(`set version ${version} in ${path.relative(REPO_ROOT, p)}\n`);
    }
  } catch (e) {
    process.stderr.write(`error: ${e.message}\n`);
    process.exit(1);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === __filename) {
  main();
}
