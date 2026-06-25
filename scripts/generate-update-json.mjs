#!/usr/bin/env node
// generate-update-json.mjs (SPEC s15 / s19.3 publish-updater-manifest, ROADMAP M9).
//
// Writes the per-target Tauri updater manifest (`update.json`) consumed by the
// in-app updater (src-tauri/src/updater.rs). For each built bundle that has a
// detached `.sig` signature, it emits one manifest laid out to match the
// endpoint URL shape the updater fetches:
//
//   updates/<channel>/<target>/<version>/update.json
//
// where <target> is the Tauri updater target triple (e.g. `windows-x86_64`,
// `darwin-aarch64`, `linux-x86_64`) - matching the `{{target}}` placeholder in
// the endpoint - and <version> matches `{{current_version}}` (the RUNNING build's
// version; the updater substitutes its own version, so the manifest for "what
// 0.1.0 should update to" lives under .../<0.1.0>/update.json).
//
// The manifest itself is the standard Tauri shape:
//   { version, notes, pub_date, platforms: { "<target>": { signature, url } } }
// (a single-target manifest per file; one file per target).
//
// PURE Node, NO network: it only reads local bundle + `.sig` files and writes
// JSON. The GitHub Actions wiring that CALLS this (uploading the bundles +
// committing the manifests) is M9d - here we just generate + unit-smoke it.
//
// Usage:
//   node scripts/generate-update-json.mjs <stable|dev> [options]
//
// Options:
//   --version <semver>   Override the version (stable defaults to the version in
//                        src-tauri/tauri.conf.json).
//   --sha <gitsha>       For the `dev` channel, the version becomes
//                        `0.0.0-dev.<sha>` unless --version is given.
//   --bundles <dir>      Directory to scan for bundle + `.sig` files
//                        (default: src-tauri/target/release/bundle).
//   --out <dir>          Output root (default: ./updates).
//   --base-url <url>     Public base URL the bundles are hosted at; the per-target
//                        `url` is `<base-url>/<bundle-filename>`. Defaults to
//                        https://github.com/pmaxhogan/driven/releases/download/v<version>
//   --notes <text>       Release notes for the manifest (default: empty).
//   --pub-date <rfc3339> Publish date (default: now).
//   --self-check         Run the built-in self-check (a temp-fixture smoke) and exit.
//   --help               Show this help.

import { promises as fs } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..");

/** The GitHub repo whose releases host the bundles (the default base URL). */
const GITHUB_REPO = "pmaxhogan/driven";

/** Map a bundle FILENAME to its Tauri updater target triple, or null if the file
 * is not an updater-eligible bundle. The updater consumes one bundle per target:
 *  - Windows: the NSIS installer (`*-setup.exe`) or the `.msi`.
 *  - macOS: the `.app.tar.gz` (the updater artifact for app bundles); the arch is
 *    in the filename (`x64`/`x86_64` vs `aarch64`/`arm64`).
 *  - Linux: the `.AppImage` (or its `.tar.gz`).
 * The `.sig` is the detached signature sitting next to the bundle. */
export function targetForBundle(filename) {
  const lower = filename.toLowerCase();
  const isArm = /(aarch64|arm64)/.test(lower);
  const isX64 = /(x86_64|x64|amd64)/.test(lower);

  // macOS app bundle updater artifact.
  if (lower.endsWith(".app.tar.gz")) {
    if (isArm) return "darwin-aarch64";
    // Default macOS app bundles to x86_64 when the arch is not in the name.
    return "darwin-x86_64";
  }
  // Windows installers.
  if (lower.endsWith(".exe") || lower.endsWith(".msi")) {
    if (isArm) return "windows-aarch64";
    return "windows-x86_64";
  }
  // Linux AppImage (and its tarball form).
  if (lower.endsWith(".appimage") || lower.endsWith(".appimage.tar.gz")) {
    if (isArm) return "linux-aarch64";
    return "linux-x86_64";
  }
  return null;
}

/** Parse argv (after the channel) into an options object. */
export function parseArgs(argv) {
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
      case "--version":
        opts.version = take();
        break;
      case "--sha":
        opts.sha = take();
        break;
      case "--bundles":
        opts.bundles = take();
        break;
      case "--out":
        opts.out = take();
        break;
      case "--base-url":
        opts.baseUrl = take();
        break;
      case "--notes":
        opts.notes = take();
        break;
      case "--pub-date":
        opts.pubDate = take();
        break;
      case "--self-check":
        opts.selfCheck = true;
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

/** Resolve the version for a channel + options. Stable defaults to the
 * tauri.conf.json version; dev becomes `0.0.0-dev.<sha>` unless overridden. */
export async function resolveVersion(channel, opts, readConfVersion) {
  if (opts.version) return opts.version;
  if (channel === "dev") {
    if (!opts.sha) {
      throw new Error("the `dev` channel requires --sha <gitsha> (or --version)");
    }
    return `0.0.0-dev.${opts.sha}`;
  }
  // stable: read from tauri.conf.json.
  return readConfVersion();
}

/** Read the `version` field from src-tauri/tauri.conf.json. */
async function readTauriConfVersion() {
  const confPath = path.join(REPO_ROOT, "src-tauri", "tauri.conf.json");
  const raw = await fs.readFile(confPath, "utf8");
  const conf = JSON.parse(raw);
  if (typeof conf.version !== "string") {
    throw new Error(`tauri.conf.json has no string \`version\`: ${confPath}`);
  }
  return conf.version;
}

/** Scan `bundlesDir` (recursively) for `.sig` files and pair each with its
 * sibling bundle, returning `[{ target, bundleFile, sigFile, signature }]`.
 * A `.sig` whose bundle filename does not map to a known target is skipped (with
 * a warning) rather than failing the whole run. */
export async function collectSignedBundles(bundlesDir, log = console) {
  const out = [];
  const seenTargets = new Set();

  async function walk(dir) {
    let entries;
    try {
      entries = await fs.readdir(dir, { withFileTypes: true });
    } catch (e) {
      throw new Error(`bundles directory unreadable: ${dir} (${e.message})`);
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        await walk(full);
        continue;
      }
      if (!entry.name.endsWith(".sig")) continue;
      const bundleFile = full.slice(0, -".sig".length);
      const bundleName = path.basename(bundleFile);
      const target = targetForBundle(bundleName);
      if (!target) {
        log.warn?.(`skip (no target mapping): ${bundleName}`);
        continue;
      }
      if (seenTargets.has(target)) {
        // Two bundles map to the same target (e.g. .exe + .msi). Keep the first
        // deterministically and warn; the updater wants exactly one per target.
        log.warn?.(`duplicate target ${target}; keeping first, skipping ${bundleName}`);
        continue;
      }
      const signature = (await fs.readFile(full, "utf8")).trim();
      if (signature.length === 0) {
        throw new Error(`empty signature file: ${full}`);
      }
      seenTargets.add(target);
      out.push({ target, bundleFile, bundleName, sigFile: full, signature });
    }
  }

  await walk(bundlesDir);
  // Deterministic order (sorted by target) so output + tests are stable.
  out.sort((a, b) => a.target.localeCompare(b.target));
  return out;
}

/** Build the per-target manifest object (the Tauri updater shape). */
export function buildManifest({ version, target, signature, url, notes, pubDate }) {
  return {
    version,
    notes: notes ?? "",
    pub_date: pubDate ?? new Date().toISOString(),
    platforms: {
      [target]: { signature, url },
    },
  };
}

/** The output path for a manifest, matching the endpoint URL shape. */
export function manifestOutPath(outRoot, channel, target, version) {
  return path.join(outRoot, channel, target, version, "update.json");
}

/** Generate all manifests. Returns the list of written file paths. */
export async function generate(channel, opts, { readConfVersion, log = console } = {}) {
  if (channel !== "stable" && channel !== "dev") {
    throw new Error(`channel must be \`stable\` or \`dev\` (got \`${channel}\`)`);
  }
  const version = await resolveVersion(channel, opts, readConfVersion ?? readTauriConfVersion);
  const bundlesDir = opts.bundles
    ? path.resolve(opts.bundles)
    : path.join(REPO_ROOT, "src-tauri", "target", "release", "bundle");
  const outRoot = opts.out ? path.resolve(opts.out) : path.join(REPO_ROOT, "updates");
  const baseUrl =
    opts.baseUrl ??
    `https://github.com/${GITHUB_REPO}/releases/download/v${version}`;
  const pubDate = opts.pubDate ?? new Date().toISOString();
  const notes = opts.notes ?? "";

  const bundles = await collectSignedBundles(bundlesDir, log);
  if (bundles.length === 0) {
    throw new Error(
      `no signed bundles (*.sig) found under ${bundlesDir}; build with bundle.createUpdaterArtifacts=true first`,
    );
  }

  const written = [];
  for (const b of bundles) {
    const url = `${baseUrl.replace(/\/$/, "")}/${b.bundleName}`;
    const manifest = buildManifest({
      version,
      target: b.target,
      signature: b.signature,
      url,
      notes,
      pubDate,
    });
    const outPath = manifestOutPath(outRoot, channel, b.target, version);
    await fs.mkdir(path.dirname(outPath), { recursive: true });
    await fs.writeFile(outPath, JSON.stringify(manifest, null, 2) + "\n", "utf8");
    written.push(outPath);
    log.info?.(`wrote ${path.relative(REPO_ROOT, outPath)} (${b.target})`);
  }
  return { version, channel, written };
}

const HELP = `generate-update-json.mjs (SPEC s15 / ROADMAP M9)

Generate per-target Tauri update.json manifests from signed bundles.

Usage:
  node scripts/generate-update-json.mjs <stable|dev> [options]

Options:
  --version <semver>    Override the version (stable defaults to tauri.conf.json).
  --sha <gitsha>        dev channel version becomes 0.0.0-dev.<sha>.
  --bundles <dir>       Directory of bundle + .sig files
                        (default: src-tauri/target/release/bundle).
  --out <dir>           Output root (default: ./updates).
  --base-url <url>      Public base URL the bundles are hosted at.
  --notes <text>        Release notes for the manifest.
  --pub-date <rfc3339>  Publish date (default: now).
  --self-check          Run the built-in temp-fixture smoke test and exit.
  --help                Show this help.

Output layout (matches the updater endpoint URL):
  updates/<channel>/<target>/<version>/update.json
`;

/** Built-in self-check: build a temp fixture of fake bundles + .sig files, run
 * the generator, and assert the emitted manifest shape + path layout. Exits
 * non-zero on failure. Used by `--self-check` and by the vitest smoke. */
export async function selfCheck() {
  const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-"));
  const bundles = path.join(tmp, "bundle");
  const out = path.join(tmp, "out");
  await fs.mkdir(path.join(bundles, "nsis"), { recursive: true });
  await fs.mkdir(path.join(bundles, "macos"), { recursive: true });

  // Fake bundles + .sig (the bytes are irrelevant - only the .sig content + the
  // filename->target mapping matter).
  await fs.writeFile(path.join(bundles, "nsis", "Driven_0.1.0_x64-setup.exe"), "fake");
  await fs.writeFile(
    path.join(bundles, "nsis", "Driven_0.1.0_x64-setup.exe.sig"),
    "SIGWIN==\n",
  );
  await fs.writeFile(path.join(bundles, "macos", "Driven_aarch64.app.tar.gz"), "fake");
  await fs.writeFile(
    path.join(bundles, "macos", "Driven_aarch64.app.tar.gz.sig"),
    "SIGMAC==\n",
  );

  const silent = { info: () => {}, warn: () => {} };
  const result = await generate(
    "stable",
    { version: "0.1.0", bundles, out, baseUrl: "https://example.test/dl" },
    { readConfVersion: async () => "0.1.0", log: silent },
  );

  const errors = [];
  if (result.version !== "0.1.0") errors.push(`version: ${result.version}`);
  if (result.written.length !== 2) errors.push(`expected 2 manifests, got ${result.written.length}`);

  // Windows manifest at the expected path + shape.
  const winPath = manifestOutPath(out, "stable", "windows-x86_64", "0.1.0");
  const win = JSON.parse(await fs.readFile(winPath, "utf8"));
  if (win.version !== "0.1.0") errors.push("win version");
  if (!win.platforms["windows-x86_64"]) errors.push("win platform key");
  if (win.platforms["windows-x86_64"]?.signature !== "SIGWIN==") errors.push("win signature");
  if (
    win.platforms["windows-x86_64"]?.url !==
    "https://example.test/dl/Driven_0.1.0_x64-setup.exe"
  ) {
    errors.push(`win url: ${win.platforms["windows-x86_64"]?.url}`);
  }

  // macOS aarch64 manifest.
  const macPath = manifestOutPath(out, "stable", "darwin-aarch64", "0.1.0");
  const mac = JSON.parse(await fs.readFile(macPath, "utf8"));
  if (mac.platforms["darwin-aarch64"]?.signature !== "SIGMAC==") errors.push("mac signature");

  await fs.rm(tmp, { recursive: true, force: true });

  if (errors.length > 0) {
    throw new Error(`self-check failed:\n  ${errors.join("\n  ")}`);
  }
  return true;
}

async function main() {
  const args = process.argv.slice(2);

  // `--help` / `-h` may appear as the first token (no channel).
  if (args.length === 0 || args[0] === "--help" || args[0] === "-h") {
    process.stdout.write(HELP);
    process.exit(args.length === 0 ? 1 : 0);
  }
  // `--self-check` may appear as the first token (no channel needed - it uses an
  // internal fixture).
  if (args.includes("--self-check")) {
    try {
      await selfCheck();
      process.stdout.write("self-check OK\n");
      process.exit(0);
    } catch (e) {
      process.stderr.write(`${e.message}\n`);
      process.exit(1);
    }
  }

  const [channel, ...rest] = args;
  let opts;
  try {
    opts = parseArgs(rest);
  } catch (e) {
    process.stderr.write(`error: ${e.message}\n\n${HELP}`);
    process.exit(2);
  }
  if (opts.help) {
    process.stdout.write(HELP);
    process.exit(0);
  }
  try {
    const result = await generate(channel, opts);
    process.stdout.write(
      `generated ${result.written.length} manifest(s) for ${result.channel} v${result.version}\n`,
    );
  } catch (e) {
    process.stderr.write(`error: ${e.message}\n`);
    process.exit(1);
  }
}

// Run main only when invoked directly (not when imported by the vitest smoke).
if (process.argv[1] && path.resolve(process.argv[1]) === __filename) {
  main();
}
