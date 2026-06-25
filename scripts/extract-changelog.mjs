#!/usr/bin/env node
// extract-changelog.mjs (ROADMAP M9 R2-P2-2).
//
// Extract a single version's section out of a Keep-a-Changelog / release-please
// CHANGELOG.md so the release pipeline can feed DETERMINISTIC, real release notes
// to BOTH the GitHub Release body AND the updater manifest (the in-app "View
// changelog"). Previously release.yml hardcoded the body to "See CHANGELOG.md for
// details." and fed THAT into the manifest, so the in-app changelog was junk
// (an M9 acceptance miss).
//
// A release-please section header looks like one of:
//   ## [0.1.1](https://github.com/owner/repo/compare/v0.1.0...v0.1.1) (2026-06-24)
//   ## [0.1.1] - 2026-06-24
//   ## 0.1.1 (2026-06-24)
//   ## [Unreleased]
// We match the FIRST `## ` heading whose version token equals the requested
// version (with or without a leading `v`), and return every line up to (but not
// including) the NEXT `## ` heading, trimmed. The `[Unreleased]` section is
// skipped unless explicitly requested.
//
// Usage:
//   node scripts/extract-changelog.mjs <version> [--file <path>] [--out <path>]
//     [--allow-empty]
// e.g.
//   node scripts/extract-changelog.mjs v0.1.1 --out release-notes.md
//
// Exit codes: 0 success; 1 the section was not found / empty (unless
// --allow-empty); 2 bad usage.

import { promises as fs } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..");

/** Normalize a version-ish token: strip a leading `v` and surrounding space. */
export function normalizeVersion(v) {
  return String(v).trim().replace(/^v/i, "");
}

/** Pull the version token out of a `## ...` heading line, or null if it is not a
 * recognizable version heading. Handles `## [1.2.3](url) (date)`, `## [1.2.3]`,
 * `## 1.2.3 (date)`, `## [Unreleased]`, etc. */
export function headingVersion(line) {
  const trimmed = line.trim();
  if (!trimmed.startsWith("## ")) return null;
  const rest = trimmed.slice(3).trim();
  // `[Unreleased]` (or any bracketed non-version label) -> its inner text.
  const bracket = rest.match(/^\[([^\]]+)\]/);
  if (bracket) return bracket[1].trim();
  // `1.2.3 (date)` or bare `1.2.3`.
  const bare = rest.match(/^v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)/);
  if (bare) return bare[1];
  return rest;
}

/** Extract the body of the section for `version` from a CHANGELOG.md string.
 * Returns the trimmed section text (NOT including its own heading), or "" when
 * the section is absent or empty. Pure (string in -> string out). */
export function extractSection(changelog, version) {
  const want = normalizeVersion(version);
  const lines = changelog.split("\n");
  let start = -1;
  for (let i = 0; i < lines.length; i++) {
    const hv = headingVersion(lines[i]);
    if (hv === null) continue;
    if (normalizeVersion(hv) === want) {
      start = i + 1;
      break;
    }
  }
  if (start === -1) return "";
  const body = [];
  for (let i = start; i < lines.length; i++) {
    if (lines[i].trim().startsWith("## ")) break;
    body.push(lines[i]);
  }
  return body.join("\n").trim();
}

/** Read CHANGELOG.md (default at the repo root) and extract `version`'s section. */
export async function extractFromFile(version, file) {
  const p = file ? path.resolve(file) : path.join(REPO_ROOT, "CHANGELOG.md");
  const raw = await fs.readFile(p, "utf8");
  return extractSection(raw, version);
}

function parseArgs(argv) {
  const opts = { version: undefined, file: undefined, out: undefined, allowEmpty: false };
  const positional = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--file") opts.file = argv[++i];
    else if (a === "--out") opts.out = argv[++i];
    else if (a === "--allow-empty") opts.allowEmpty = true;
    else if (a === "--help" || a === "-h") opts.help = true;
    else positional.push(a);
  }
  opts.version = positional[0];
  return opts;
}

const USAGE =
  "usage: node scripts/extract-changelog.mjs <version> [--file <path>] " +
  "[--out <path>] [--allow-empty]\n";

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    process.stdout.write(USAGE);
    process.exit(0);
  }
  if (!opts.version) {
    process.stderr.write(USAGE);
    process.exit(2);
  }
  let section;
  try {
    section = await extractFromFile(opts.version, opts.file);
  } catch (e) {
    process.stderr.write(`error: ${e.message}\n`);
    process.exit(1);
  }
  if (!section && !opts.allowEmpty) {
    process.stderr.write(
      `error: no non-empty CHANGELOG.md section found for version ${opts.version}\n`,
    );
    process.exit(1);
  }
  if (opts.out) {
    await fs.writeFile(path.resolve(opts.out), section + "\n", "utf8");
    process.stderr.write(`wrote ${section.length} chars to ${opts.out}\n`);
  } else {
    process.stdout.write(section + "\n");
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === __filename) {
  main();
}
