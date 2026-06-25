#!/usr/bin/env node
// set-dev-version.mjs (ROADMAP M9 R1-P1-5).
//
// Patch the app version into ALL canonical version sources BEFORE a dev-channel
// Tauri build, so the produced app actually reports `0.0.0-dev.<sha>`. Without
// this the dev build still reports the stable version, so the updater's version
// comparison (running vs manifest) and `{{current_version}}` are wrong and a dev
// user would never see a newer dev build (or would see a spurious downgrade).
//
// The three sources release-please bumps in lockstep (release-please-config.json)
// are the same three we patch here, so the dev build mirrors a real release:
//   1. root Cargo.toml  [workspace.package].version  (src-tauri uses
//      version.workspace = true, so this is THE Rust/app version)
//   2. src-tauri/tauri.conf.json  .version
//   3. ui/package.json  .version
//
// Usage:
//   node scripts/set-dev-version.mjs <new-version>
// e.g.
//   node scripts/set-dev-version.mjs 0.0.0-dev.abc1234

import { promises as fs } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..");

/** Validate a SemVer-ish version (incl. the 0.0.0-dev.<sha> prerelease form). */
export function isValidVersion(v) {
  return /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(v);
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

async function main() {
  const version = process.argv[2];
  if (!version) {
    process.stderr.write("usage: node scripts/set-dev-version.mjs <new-version>\n");
    process.exit(2);
  }
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
