#!/usr/bin/env node
// generate-update-json.mjs (SPEC s15 / s19.3 publish-updater-manifest, ROADMAP M9).
//
// Writes the per-target Tauri updater manifest (`update.json`) consumed by the
// in-app updater (src-tauri/src/updater.rs). For each built bundle that has a
// detached `.sig` signature, it emits one manifest laid out to match the
// endpoint URL shape the updater fetches:
//
//   updates/<channel>/<os>/<arch>/update.json
//
// where <os> is the Tauri `{{target}}` placeholder (`windows` | `darwin` |
// `linux`) and <arch> is the `{{arch}}` placeholder (`x86_64` | `aarch64`).
// The path carries NO version segment: per Tauri's static-server model the
// manifest itself carries the latest version and the updater compares its
// running version to it (R1-P1-1 - including `{{current_version}}` made a 0.1.0
// app look under /0.1.0/ while a 0.1.1 release wrote under /0.1.1/, so updates
// were never discovered). The channel lives in the PATH ({{channel}} is NOT a
// valid Tauri placeholder).
//
// The manifest itself is the standard Tauri shape, keyed by the COMBINED
// `<os>-<arch>` platform key Tauri matches at runtime:
//   { version, notes, pub_date, platforms: { "<os>-<arch>": { signature, url } } }
// (a single-platform manifest per file; one file per os/arch).
//
// PURE Node, NO network: it only reads local bundle + `.sig` files (the assets
// dir the workflow populates via `gh release download`) and writes JSON. The
// GitHub Actions wiring that CALLS this (downloading the release assets +
// deploying the manifests) lives in release.yml / dev-channel.yml.
//
// Usage:
//   node scripts/generate-update-json.mjs <stable|dev> [options]
//
// Options:
//   --version <semver>   The manifest version. Stable defaults to the version in
//                        src-tauri/tauri.conf.json. The `dev` channel REQUIRES it
//                        (the workflow computes it once via
//                        `set-dev-version.mjs --print-dev-version` and threads the
//                        SAME monotonic value here - R3-P2-1).
//   --run-number <n>     dev only: with --dev-sha, derive the version from the
//   --dev-sha <sha>      SHARED set-dev-version logic (computeDevVersionFromRepo)
//                        when no precomputed --version is supplied (a manual run).
//                        There is NO `0.0.0-dev.<sha>` form anymore (R3-P2-1).
//   --require-targets <list>
//                        Comma/space separated combined `<os>-<arch>` keys (e.g.
//                        `windows-x86_64,darwin-x86_64,darwin-aarch64,linux-x86_64`)
//                        that MUST each produce a manifest, else the run ERRORS
//                        (R3-P1-2 - blocks a partial update tree from publishing).
//   --bundles <dir>      Directory to scan for bundle + `.sig` files
//                        (default: src-tauri/target/release/bundle). Alias:
//                        --assets-dir (the workflow downloads release assets
//                        into a flat dir and points this at it).
//   --assets-dir <dir>   Alias for --bundles.
//   --out <dir>          Output root (default: ./updates).
//   --base-url <url>     Public base URL the bundles are hosted at; the per-target
//                        `url` is `<base-url>/<bundle-filename>`. Defaults to
//                        https://github.com/pmaxhogan/driven/releases/download/v<version>
//   --notes <text>       Release notes for the manifest (default: empty).
//   --notes-file <path>  Read the release notes from a file (overrides --notes).
//   --pub-date <rfc3339> Publish date (default: now).
//   --self-check         Run the built-in self-check (a temp-fixture smoke) and exit.
//   --help               Show this help.

import { promises as fs } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";

// R3-P2-1: the dev version has ONE source of truth - set-dev-version.mjs's
// monotonic `<next-patch>-dev.<run_number>.<sha>` logic. The generator must NOT
// re-implement a contradictory `0.0.0-dev.<sha>` form (that published a dev
// manifest BELOW stable, so opted-in users were never offered the update). We
// import the shared helpers so a manual `dev` run computes the SAME value the
// dev-channel workflow patches into the app metadata + bundles.
import {
  computeDevVersionFromRepo,
  isValidVersion,
} from "./set-dev-version.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..");

/** The GitHub repo whose releases host the bundles (the default base URL). */
const GITHUB_REPO = "pmaxhogan/driven";

/** The EXACT V1 GA updater target set (R3-P1-2). Every channel release MUST
 * produce a manifest for each of these or the publish is a partial (potentially
 * bricking) update tree. Kept as the single source of truth used by
 * [`assertRequiredTargets`] and the `--require-targets` default. */
export const V1_REQUIRED_TARGETS = [
  "windows-x86_64",
  "darwin-x86_64",
  "darwin-aarch64",
  "linux-x86_64",
];

/** Map a bundle FILENAME to its Tauri updater target triple, or null if the file
 * is not an updater-eligible bundle. The updater consumes one bundle per target:
 *  - Windows: the NSIS installer (`*-setup.exe`) or the `.msi`.
 *  - macOS: the `.app.tar.gz` (the updater artifact for app bundles); the arch is
 *    in the filename (`x64`/`x86_64` vs `aarch64`/`arm64`).
 *  - Linux: the `.AppImage` (or its `.tar.gz`).
 * The `.sig` is the detached signature sitting next to the bundle.
 *
 * R3-P1-1: a macOS `.app.tar.gz` carries NO arch in tauri's default on-disk name
 * (it is named from the `.app` bundle, e.g. `Driven.app.tar.gz`), so BOTH the
 * aarch64 and x86_64 mac jobs would emit the same basename and collide in a flat
 * release asset set - silently dropping one arch or advertising ARM as x86_64.
 * The workflows now force the arch into the mac asset/bundle name BEFORE upload
 * (release.yml via tauri-action `releaseAssetNamePattern` with `[arch]`;
 * dev-channel.yml by renaming the collected bundle+sig per `matrix.target`). To
 * make that contract enforced rather than assumed, an ARCHLESS mac updater
 * bundle is REJECTED (throws) here instead of defaulting to x86_64. */
export function targetForBundle(filename) {
  const lower = filename.toLowerCase();
  const isArm = /(aarch64|arm64)/.test(lower);
  const isX86 = /(x86_64|x64|amd64|intel)/.test(lower);

  // macOS app bundle updater artifact.
  if (lower.endsWith(".app.tar.gz")) {
    if (isArm) return "darwin-aarch64";
    if (isX86) return "darwin-x86_64";
    // R3-P1-1: refuse to guess the arch of an archless mac updater bundle - the
    // workflow MUST stamp the arch into the name (Driven_<arch>.app.tar.gz or
    // [name]_[version]_[platform]_[arch].app.tar.gz). Defaulting to x86_64 would
    // mis-advertise an Apple-silicon build as Intel.
    throw new Error(
      `archless macOS updater bundle: ${filename}; the release/dev workflow must ` +
        `stamp the arch into the name (e.g. Driven_aarch64.app.tar.gz) so each mac ` +
        `arch maps to a distinct target (R3-P1-1)`,
    );
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

/** Split a combined Tauri platform key (`<os>-<arch>`, e.g. `darwin-aarch64`)
 * into its `{{target}}` OS segment and `{{arch}}` architecture segment - the two
 * path segments the updater endpoint substitutes. The combined form remains the
 * `platforms` map KEY (what Tauri matches at runtime); the split is only for the
 * directory layout `updates/<channel>/<os>/<arch>/update.json`. */
export function osArchForTarget(target) {
  const idx = target.indexOf("-");
  if (idx <= 0 || idx === target.length - 1) {
    throw new Error(`malformed platform key (expected <os>-<arch>): ${target}`);
  }
  return { os: target.slice(0, idx), arch: target.slice(idx + 1) };
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
      case "--require-targets":
        opts.requireTargets = take();
        break;
      case "--run-number":
        opts.runNumber = take();
        break;
      case "--dev-sha":
        opts.devSha = take();
        break;
      case "--bundles":
      case "--assets-dir":
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
      case "--notes-file":
        opts.notesFile = take();
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

/** Resolve the version for a channel + options.
 *
 * stable: the version in tauri.conf.json (release-please keeps it in lockstep
 * with the tag).
 *
 * dev (R3-P2-1): the version is the SHARED monotonic `<next-patch>-dev.<run>.<sha>`
 * value owned by set-dev-version.mjs - NEVER the old `0.0.0-dev.<sha>` form. The
 * dev-channel workflow computes it once (`set-dev-version.mjs --print-dev-version`)
 * and threads the SAME value into the app metadata AND here via `--version`, so
 * `--version` is the normal path. For a manual operator run WITHOUT a precomputed
 * value, pass `--run-number <n> --dev-sha <sha>` and the generator delegates to
 * the same `computeDevVersionFromRepo` helper (the ONE source of truth) rather
 * than re-deriving a contradictory version here.
 *
 * `computeDev` is injectable for tests (defaults to the real shared helper). */
export async function resolveVersion(
  channel,
  opts,
  readConfVersion,
  computeDev = computeDevVersionFromRepo,
) {
  if (opts.version) {
    if (!isValidVersion(opts.version)) {
      throw new Error(`invalid --version: ${opts.version}`);
    }
    return opts.version;
  }
  if (channel === "dev") {
    if (opts.runNumber !== undefined && opts.devSha !== undefined) {
      // Delegate to the shared monotonic logic (set-dev-version.mjs).
      return computeDev(opts.runNumber, opts.devSha);
    }
    throw new Error(
      "the `dev` channel requires --version <semver> (computed once via " +
        "`set-dev-version.mjs --print-dev-version <run> <sha>` in the workflow), " +
        "or --run-number <n> --dev-sha <sha> to derive it from the shared logic",
    );
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

/** Extract a SemVer-shaped version token from a bundle filename, or null if the
 * name carries none (e.g. a macOS `*.app.tar.gz` is typically version-less).
 * Used by [`collectSignedBundles`] to detect a STALE accreted bundle (R2-P1-2):
 * a rolling dev release keeps old assets, and a manifest that advertises the new
 * version while pointing at an old signed bundle is an integrity bug. */
export function versionFromBundleName(filename) {
  // Match <x>.<y>.<z> with an optional `-prerelease` (the dev form carries
  // `-dev.<run>.<sha>`). Greedy on the prerelease so `0.1.1-dev.5.abc` is whole.
  const m = filename.match(/(\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?)/);
  return m ? m[1] : null;
}

/** Rank two same-target candidates so the kept one is DETERMINISTIC for a
 * legitimate single-build duplicate (Windows emits BOTH `.msi` and NSIS
 * `-setup.exe`, each mapping to `windows-x86_64`). We prefer the NSIS installer
 * (`.exe`) over the `.msi` - matching tauri-action's Windows updater default -
 * then fall back to a stable filename sort. Returns the bundle to KEEP. */
function preferWindowsInstaller(a, b) {
  const isExe = (n) => n.toLowerCase().endsWith(".exe");
  if (isExe(a.bundleName) && !isExe(b.bundleName)) return a;
  if (isExe(b.bundleName) && !isExe(a.bundleName)) return b;
  return a.bundleName.localeCompare(b.bundleName) <= 0 ? a : b;
}

/** Scan `bundlesDir` (recursively) for `.sig` files and pair each with its
 * sibling bundle, returning `[{ target, bundleFile, sigFile, signature }]`.
 *
 * A `.sig` whose bundle filename does not map to a known target is skipped (with
 * a warning) rather than failing the whole run.
 *
 * R2-P1-2: when two bundles map to the SAME target the function MUST NOT
 * silently keep the first (that let a manifest advertise a new version while
 * pointing at an OLD signed bundle accreted on the rolling dev release). The
 * resolution is:
 *  - if the two candidates carry DIFFERENT parseable versions, ERROR (a stale
 *    accreted bundle - the directory was not cleaned to the current run); when
 *    `expectedVersion` is given, also ERROR if a candidate's version does not
 *    match it.
 *  - if both are the current/same version (or version-less, e.g. macOS), this is
 *    the legitimate Windows `.msi` + NSIS `.exe` pair: keep ONE deterministically
 *    (prefer NSIS) and warn.
 */
export async function collectSignedBundles(bundlesDir, log = console, expectedVersion = null) {
  // First pass: gather EVERY signed bundle, grouped by target.
  const byTarget = new Map();

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
      const signature = (await fs.readFile(full, "utf8")).trim();
      if (signature.length === 0) {
        throw new Error(`empty signature file: ${full}`);
      }
      const version = versionFromBundleName(bundleName);
      const candidate = { target, bundleFile, bundleName, sigFile: full, signature, version };
      const list = byTarget.get(target);
      if (list) list.push(candidate);
      else byTarget.set(target, [candidate]);
    }
  }

  await walk(bundlesDir);

  // Second pass: resolve each target to exactly one bundle, ERRORing on a stale
  // mismatch (R2-P1-2) rather than silently keeping the first.
  const out = [];
  for (const [target, candidates] of byTarget) {
    // If we know the expected version, any candidate carrying a DIFFERENT
    // parseable version is a stale accreted asset - fail loudly.
    if (expectedVersion) {
      const stale = candidates.find((c) => c.version !== null && c.version !== expectedVersion);
      if (stale) {
        throw new Error(
          `stale bundle for target ${target}: ${stale.bundleName} is version ` +
            `${stale.version} but the manifest version is ${expectedVersion}; the ` +
            `assets dir was not cleaned to the current run (R2-P1-2). Delete prior ` +
            `release assets before regenerating.`,
        );
      }
    }
    if (candidates.length === 1) {
      out.push(candidates[0]);
      continue;
    }
    // Multiple candidates for one target. They must all share a version (a real
    // duplicate from ONE build, e.g. Windows .msi + NSIS .exe). Differing
    // parseable versions => stale accretion => ERROR.
    const versions = new Set(candidates.map((c) => c.version).filter((v) => v !== null));
    if (versions.size > 1) {
      const detail = candidates.map((c) => `${c.bundleName} (${c.version ?? "no-version"})`).join(", ");
      throw new Error(
        `conflicting bundles for target ${target}: ${detail}; refusing to guess - ` +
          `clean the assets dir to a single run (R2-P1-2).`,
      );
    }
    // Same version (or version-less): the legitimate msi+nsis Windows pair. Keep
    // one deterministically (prefer NSIS) and warn so the choice is visible.
    const kept = candidates.reduce((acc, c) => preferWindowsInstaller(acc, c));
    for (const c of candidates) {
      if (c !== kept) {
        log.warn?.(`duplicate target ${target}; keeping ${kept.bundleName}, skipping ${c.bundleName}`);
      }
    }
    out.push(kept);
  }

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

/** The output path for a manifest, matching the endpoint URL shape
 * `updates/<channel>/<os>/<arch>/update.json` (R1-P1-1: NO version segment).
 * `target` is the combined `<os>-<arch>` platform key. */
export function manifestOutPath(outRoot, channel, target) {
  const { os: targetOs, arch } = osArchForTarget(target);
  return path.join(outRoot, channel, targetOs, arch, "update.json");
}

/** Parse a `--require-targets` value (comma/space separated combined `<os>-<arch>`
 * keys) into a deduped, validated list. An empty/whitespace value is an error
 * (the caller asked to require targets but named none). */
export function parseRequiredTargets(raw) {
  const list = String(raw)
    .split(/[,\s]+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  if (list.length === 0) {
    throw new Error("--require-targets given but no targets named");
  }
  for (const t of list) {
    // Validate the shape (throws on a malformed key); we ignore the result.
    osArchForTarget(t);
  }
  return [...new Set(list)];
}

/** R3-P1-2: fail unless EVERY required target produced a manifest. A partial
 * updater tree (a missing `.sig`, an asset collision, a mapping miss) otherwise
 * deploys silently while CI stays green and bricks/strands the missing arch's
 * auto-update. `produced` is the list of combined `<os>-<arch>` keys actually
 * generated; `required` is the must-have set. */
export function assertRequiredTargets(required, produced) {
  const have = new Set(produced);
  const missing = required.filter((t) => !have.has(t));
  if (missing.length > 0) {
    throw new Error(
      `incomplete updater target set: missing [${missing.join(", ")}] ` +
        `(produced [${[...have].sort().join(", ")}]); refusing to publish a ` +
        `PARTIAL update tree (R3-P1-2). Required: [${required.join(", ")}]`,
    );
  }
}

/** Generate all manifests. Returns the list of written file paths. */
export async function generate(
  channel,
  opts,
  { readConfVersion, log = console, computeDev } = {},
) {
  if (channel !== "stable" && channel !== "dev") {
    throw new Error(`channel must be \`stable\` or \`dev\` (got \`${channel}\`)`);
  }
  const version = await resolveVersion(
    channel,
    opts,
    readConfVersion ?? readTauriConfVersion,
    computeDev ?? computeDevVersionFromRepo,
  );
  const bundlesDir = opts.bundles
    ? path.resolve(opts.bundles)
    : path.join(REPO_ROOT, "src-tauri", "target", "release", "bundle");
  const outRoot = opts.out ? path.resolve(opts.out) : path.join(REPO_ROOT, "updates");
  // The default download base: stable assets live on the `v<version>` tag, but
  // the `dev` channel publishes to a single ROLLING `dev` GH release (the bundle
  // version is 0.0.0-dev.<sha> but the tag is just `dev`), so its assets are at
  // /releases/download/dev (R1-P1-4). An explicit --base-url overrides either.
  const baseUrl =
    opts.baseUrl ??
    (channel === "dev"
      ? `https://github.com/${GITHUB_REPO}/releases/download/dev`
      : `https://github.com/${GITHUB_REPO}/releases/download/v${version}`);
  const pubDate = opts.pubDate ?? new Date().toISOString();
  // Release notes for the in-app changelog (R1-P1-6): --notes-file wins over
  // --notes (the workflow writes `gh release view --json body` to a file to
  // avoid shell-quoting a multi-line body), then the inline --notes, else "".
  let notes = opts.notes ?? "";
  if (opts.notesFile) {
    notes = (await fs.readFile(path.resolve(opts.notesFile), "utf8")).trim();
  }

  // Pass the resolved version so a STALE accreted bundle (a prior dev run's
  // asset still on the rolling release) is rejected, not silently published
  // (R2-P1-2).
  const bundles = await collectSignedBundles(bundlesDir, log, version);
  if (bundles.length === 0) {
    throw new Error(
      `no signed bundles (*.sig) found under ${bundlesDir}; build with bundle.createUpdaterArtifacts=true first`,
    );
  }

  const written = [];
  const producedTargets = [];
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
    const outPath = manifestOutPath(outRoot, channel, b.target);
    await fs.mkdir(path.dirname(outPath), { recursive: true });
    await fs.writeFile(outPath, JSON.stringify(manifest, null, 2) + "\n", "utf8");
    written.push(outPath);
    producedTargets.push(b.target);
    log.info?.(`wrote ${path.relative(REPO_ROOT, outPath)} (${b.target})`);
  }

  // R3-P1-2: if the caller declared a required target set, EVERY one must have
  // produced a manifest or we refuse to return success (so the workflow never
  // uploads/deploys a partial update tree).
  if (opts.requireTargets !== undefined) {
    const required = parseRequiredTargets(opts.requireTargets);
    assertRequiredTargets(required, producedTargets);
  }

  return { version, channel, written };
}

const HELP = `generate-update-json.mjs (SPEC s15 / ROADMAP M9)

Generate per-target Tauri update.json manifests from signed bundles.

Usage:
  node scripts/generate-update-json.mjs <stable|dev> [options]

Options:
  --version <semver>    The manifest version. Stable defaults to tauri.conf.json.
                        REQUIRED for the dev channel (the workflow threads the
                        shared monotonic <next-patch>-dev.<run>.<sha> value here).
  --run-number <n>      dev only: with --dev-sha, derive the version from the
  --dev-sha <sha>       shared set-dev-version logic when no --version is given.
  --require-targets <l> Comma/space separated <os>-<arch> keys that MUST each
                        produce a manifest, else ERROR (e.g.
                        windows-x86_64,darwin-x86_64,darwin-aarch64,linux-x86_64).
  --bundles <dir>       Directory of bundle + .sig files
                        (default: src-tauri/target/release/bundle).
  --assets-dir <dir>    Alias for --bundles.
  --out <dir>           Output root (default: ./updates).
  --base-url <url>      Public base URL the bundles are hosted at.
  --notes <text>        Release notes for the manifest.
  --notes-file <path>   Read release notes from a file (overrides --notes).
  --pub-date <rfc3339>  Publish date (default: now).
  --self-check          Run the built-in temp-fixture smoke test and exit.
  --help                Show this help.

Output layout (matches the updater endpoint URL):
  updates/<channel>/<os>/<arch>/update.json
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
    {
      version: "0.1.0",
      bundles,
      out,
      baseUrl: "https://example.test/dl",
      notes: "Release notes for 0.1.0.",
    },
    { readConfVersion: async () => "0.1.0", log: silent },
  );

  const errors = [];
  if (result.version !== "0.1.0") errors.push(`version: ${result.version}`);
  if (result.written.length !== 2) errors.push(`expected 2 manifests, got ${result.written.length}`);

  // Windows manifest at the expected path + shape (NO version segment).
  const winPath = manifestOutPath(out, "stable", "windows-x86_64");
  if (!winPath.endsWith(path.join("stable", "windows", "x86_64", "update.json"))) {
    errors.push(`win path layout: ${winPath}`);
  }
  const win = JSON.parse(await fs.readFile(winPath, "utf8"));
  if (win.version !== "0.1.0") errors.push("win version");
  if (!win.platforms["windows-x86_64"]) errors.push("win platform key");
  if (win.platforms["windows-x86_64"]?.signature !== "SIGWIN==") errors.push("win signature");
  if (win.notes !== "Release notes for 0.1.0.") errors.push(`win notes: ${win.notes}`);
  if (
    win.platforms["windows-x86_64"]?.url !==
    "https://example.test/dl/Driven_0.1.0_x64-setup.exe"
  ) {
    errors.push(`win url: ${win.platforms["windows-x86_64"]?.url}`);
  }

  // macOS aarch64 manifest.
  const macPath = manifestOutPath(out, "stable", "darwin-aarch64");
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
