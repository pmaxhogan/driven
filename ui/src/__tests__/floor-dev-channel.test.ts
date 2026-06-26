import { describe, it, expect } from "vitest";
import { promises as fs } from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";

// floor-dev-channel.mjs unit tests (design:
// docs/superpowers/specs/2026-06-25-dev-channel-floor-design.md). Imports the
// pure functions from the script (no network) and exercises the SemVer
// precedence rule plus the per-target copy/keep/seed floor over a temp fixture of
// fake `update.json` files.

const __filename = fileURLToPath(import.meta.url);
const SCRIPT = path.resolve(path.dirname(__filename), "../../../scripts/floor-dev-channel.mjs");

const mod = await import(SCRIPT);

/** Write a minimal Tauri-shaped manifest for `target` at the channel tree path. */
async function writeManifest(root: string, channel: string, plat: string, version: string) {
  const [targetOs, arch] = plat.split("/");
  const key = `${targetOs}-${arch}`;
  const dir = path.join(root, channel, plat);
  await fs.mkdir(dir, { recursive: true });
  const body = {
    version,
    notes: `notes for ${version}`,
    pub_date: "2026-06-25T00:00:00.000Z",
    platforms: {
      [key]: {
        signature: `sig-${channel}-${plat}-${version}`,
        url: `https://example.test/${channel}/${plat}/${version}`,
      },
    },
  };
  await fs.writeFile(path.join(dir, "update.json"), JSON.stringify(body, null, 2) + "\n", "utf8");
}

async function readVersion(root: string, channel: string, plat: string): Promise<string> {
  const raw = await fs.readFile(path.join(root, channel, plat, "update.json"), "utf8");
  return JSON.parse(raw).version;
}

async function readManifest(root: string, channel: string, plat: string) {
  const raw = await fs.readFile(path.join(root, channel, plat, "update.json"), "utf8");
  return JSON.parse(raw);
}

async function fileExists(p: string): Promise<boolean> {
  try {
    await fs.stat(p);
    return true;
  } catch {
    return false;
  }
}

const silent = { info: () => {}, warn: () => {} };

describe("floor-dev-channel.mjs comparePrecedence", () => {
  it("ranks a clean release above a same-core prerelease (the load-bearing rule)", () => {
    expect(mod.comparePrecedence("0.2.0", "0.2.0-dev.30.abc1234")).toBe(1);
    expect(mod.comparePrecedence("0.2.0-dev.30.abc1234", "0.2.0")).toBe(-1);
  });

  it("compares core numerically, not lexically", () => {
    expect(mod.comparePrecedence("0.2.0", "0.1.1-dev.24.ac5e487")).toBe(1);
    expect(mod.comparePrecedence("0.2.0", "0.10.0")).toBe(-1);
    expect(mod.comparePrecedence("0.2.0", "0.2.0")).toBe(0);
  });

  it("orders dev.<n> prerelease identifiers numerically", () => {
    expect(mod.comparePrecedence("0.2.1-dev.30.abc", "0.2.1-dev.24.def")).toBe(1);
    expect(mod.comparePrecedence("0.2.1-dev.24.def", "0.2.1-dev.30.abc")).toBe(-1);
  });

  it("does not throw on an all-numeric short SHA identifier (leading-zero tolerant)", () => {
    // A git short SHA can be all digits, e.g. `0042`; it must compare numerically,
    // not throw as an invalid SemVer numeric identifier.
    expect(() => mod.comparePrecedence("0.2.1-dev.5.0042", "0.2.1-dev.5.0043")).not.toThrow();
    expect(mod.comparePrecedence("0.2.1-dev.5.0042", "0.2.1-dev.5.0043")).toBe(-1);
  });

  it("strips a leading v (release tags are v*, manifests are plain)", () => {
    expect(mod.comparePrecedence("v0.2.0", "0.2.0")).toBe(0);
    expect(mod.comparePrecedence("v0.3.0", "0.2.0")).toBe(1);
  });
});

describe("floor-dev-channel.mjs floorChannel", () => {
  it("floors dev up to stable when stable is newer, copying the stable manifest verbatim", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const plat = "windows/x86_64";
    await writeManifest(root, "stable", plat, "0.2.0");
    await writeManifest(root, "dev", plat, "0.1.1-dev.24.ac5e487");

    const counts = await mod.floorChannel({
      stableDir: path.join(root, "stable"),
      devDir: path.join(root, "dev"),
      platforms: [plat],
      log: silent,
    });

    expect(counts.floored).toBe(1);
    expect(await readVersion(root, "dev", plat)).toBe("0.2.0");
    // Body copied verbatim: the dev manifest now equals the stable one.
    expect(await readManifest(root, "dev", plat)).toEqual(await readManifest(root, "stable", plat));
    await fs.rm(root, { recursive: true, force: true });
  });

  it("keeps dev untouched when dev is already ahead of stable", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const plat = "linux/x86_64";
    await writeManifest(root, "stable", plat, "0.2.0");
    await writeManifest(root, "dev", plat, "0.2.1-dev.30.beef123");

    const counts = await mod.floorChannel({
      stableDir: path.join(root, "stable"),
      devDir: path.join(root, "dev"),
      platforms: [plat],
      log: silent,
    });

    expect(counts.kept).toBe(1);
    expect(await readVersion(root, "dev", plat)).toBe("0.2.1-dev.30.beef123");
    await fs.rm(root, { recursive: true, force: true });
  });

  it("seeds dev from stable on first publish (dev manifest absent)", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const plat = "darwin/aarch64";
    await writeManifest(root, "stable", plat, "0.2.0");

    const counts = await mod.floorChannel({
      stableDir: path.join(root, "stable"),
      devDir: path.join(root, "dev"),
      platforms: [plat],
      log: silent,
    });

    expect(counts.seeded).toBe(1);
    expect(await fileExists(path.join(root, "dev", plat, "update.json"))).toBe(true);
    expect(await readVersion(root, "dev", plat)).toBe("0.2.0");
    await fs.rm(root, { recursive: true, force: true });
  });

  it("leaves dev as-is when there is no stable manifest to floor against", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const plat = "darwin/x86_64";
    await writeManifest(root, "dev", plat, "0.2.1-dev.5.abc");

    const counts = await mod.floorChannel({
      stableDir: path.join(root, "stable"),
      devDir: path.join(root, "dev"),
      platforms: [plat],
      log: silent,
    });

    expect(counts.missingStable).toBe(1);
    expect(await readVersion(root, "dev", plat)).toBe("0.2.1-dev.5.abc");
    await fs.rm(root, { recursive: true, force: true });
  });

  it("handles a full 4-target tree mixing floor, keep, seed, and missing-stable", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const platforms = ["windows/x86_64", "darwin/x86_64", "darwin/aarch64", "linux/x86_64"];
    // windows: dev behind -> floor.
    await writeManifest(root, "stable", "windows/x86_64", "0.2.0");
    await writeManifest(root, "dev", "windows/x86_64", "0.1.1-dev.24.ac5e487");
    // darwin/x86_64: dev ahead -> keep.
    await writeManifest(root, "stable", "darwin/x86_64", "0.2.0");
    await writeManifest(root, "dev", "darwin/x86_64", "0.2.1-dev.30.beef");
    // darwin/aarch64: dev missing -> seed.
    await writeManifest(root, "stable", "darwin/aarch64", "0.2.0");
    // linux: stable missing -> leave dev.
    await writeManifest(root, "dev", "linux/x86_64", "0.2.1-dev.5.abc");

    const counts = await mod.floorChannel({
      stableDir: path.join(root, "stable"),
      devDir: path.join(root, "dev"),
      platforms,
      log: silent,
    });

    expect(counts).toEqual({ floored: 1, kept: 1, seeded: 1, missingStable: 1 });
    expect(await readVersion(root, "dev", "windows/x86_64")).toBe("0.2.0");
    expect(await readVersion(root, "dev", "darwin/x86_64")).toBe("0.2.1-dev.30.beef");
    expect(await readVersion(root, "dev", "darwin/aarch64")).toBe("0.2.0");
    expect(await readVersion(root, "dev", "linux/x86_64")).toBe("0.2.1-dev.5.abc");
    await fs.rm(root, { recursive: true, force: true });
  });
});

describe("floor-dev-channel.mjs assertFloored", () => {
  it("passes once every target has dev >= stable", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const plat = "windows/x86_64";
    await writeManifest(root, "stable", plat, "0.2.0");
    await writeManifest(root, "dev", plat, "0.2.0");
    await expect(
      mod.assertFloored({
        stableDir: path.join(root, "stable"),
        devDir: path.join(root, "dev"),
        platforms: [plat],
      })
    ).resolves.toBeUndefined();
    await fs.rm(root, { recursive: true, force: true });
  });

  it("throws naming the offending target when dev is below stable", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "floor-"));
    const plat = "windows/x86_64";
    await writeManifest(root, "stable", plat, "0.2.0");
    await writeManifest(root, "dev", plat, "0.1.1-dev.24.ac5e487");
    await expect(
      mod.assertFloored({
        stableDir: path.join(root, "stable"),
        devDir: path.join(root, "dev"),
        platforms: [plat],
      })
    ).rejects.toThrow(/windows\/x86_64.*0\.1\.1-dev\.24\.ac5e487 < stable=0\.2\.0/);
    await fs.rm(root, { recursive: true, force: true });
  });
});
