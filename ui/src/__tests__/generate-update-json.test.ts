import { describe, it, expect } from "vitest";
import { promises as fs } from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";

// generate-update-json.mjs smoke test (SPEC s15 / s19.3; ROADMAP M9). Imports the
// pure functions from the script (no network, no real bundles) and asserts the
// emitted update.json SHAPE + PATH LAYOUT against a temp fixture of fake
// bundles+.sig - the exact contract the in-app updater's endpoint URL expects
// (R1-P1-1: NO version segment - the manifest carries the latest version):
//   updates/<channel>/<os>/<arch>/update.json
// with a `{ version, notes, pub_date, platforms: { <os>-<arch>: { signature, url } } }`
// manifest.

const __filename = fileURLToPath(import.meta.url);
const SCRIPT = path.resolve(
  path.dirname(__filename),
  "../../../scripts/generate-update-json.mjs",
);

const mod = await import(SCRIPT);

describe("generate-update-json.mjs", () => {
  it("maps bundle filenames to the correct Tauri target triples", () => {
    expect(mod.targetForBundle("Driven_0.1.0_x64-setup.exe")).toBe(
      "windows-x86_64",
    );
    expect(mod.targetForBundle("Driven_0.1.0_arm64-setup.exe")).toBe(
      "windows-aarch64",
    );
    expect(mod.targetForBundle("Driven_aarch64.app.tar.gz")).toBe(
      "darwin-aarch64",
    );
    expect(mod.targetForBundle("Driven_x64.app.tar.gz")).toBe("darwin-x86_64");
    // R3-P1-1: the release.yml [arch] / dev-channel rename forms both map.
    expect(
      mod.targetForBundle("Driven_0.1.0_darwin_aarch64.app.tar.gz"),
    ).toBe("darwin-aarch64");
    expect(
      mod.targetForBundle("Driven_0.1.0_darwin_x86_64.app.tar.gz"),
    ).toBe("darwin-x86_64");
    expect(mod.targetForBundle("driven_0.1.0_amd64.AppImage")).toBe(
      "linux-x86_64",
    );
    // A non-updater artifact maps to null (skipped).
    expect(mod.targetForBundle("Driven_0.1.0_amd64.deb")).toBeNull();
  });

  it("R3-P1-1: REJECTS an archless macOS updater bundle instead of defaulting to x86_64", () => {
    // The default tauri on-disk name (`Driven.app.tar.gz`) has no arch; both mac
    // jobs would collide. The generator must refuse to guess, not silently
    // classify it as Intel.
    expect(() => mod.targetForBundle("Driven.app.tar.gz")).toThrow(
      /archless macOS/,
    );
  });

  it("resolves the stable version from the conf reader", async () => {
    const v = await mod.resolveVersion("stable", {}, async () => "0.1.0");
    expect(v).toBe("0.1.0");
  });

  it("uses an explicit --version verbatim (the dev workflow's normal path)", async () => {
    const v = await mod.resolveVersion(
      "dev",
      { version: "0.1.1-dev.123.abc1234" },
      async () => "9.9.9",
    );
    expect(v).toBe("0.1.1-dev.123.abc1234");
  });

  it("R3-P2-1: dev version delegates to the SHARED set-dev-version logic (no 0.0.0-dev.<sha>)", async () => {
    // No precomputed --version: with --run-number + --dev-sha the generator must
    // delegate to the injected shared computeDev helper (computeDevVersionFromRepo
    // in production) - NOT re-implement a below-stable 0.0.0-dev.<sha> version.
    const calls: Array<[unknown, unknown]> = [];
    const computeDev = async (run: unknown, sha: unknown) => {
      calls.push([run, sha]);
      return "0.1.1-dev.42.cafe123";
    };
    const v = await mod.resolveVersion(
      "dev",
      { runNumber: "42", devSha: "cafe123" },
      async () => "9.9.9",
      computeDev,
    );
    expect(v).toBe("0.1.1-dev.42.cafe123");
    expect(calls).toEqual([["42", "cafe123"]]);
    // The version is ABOVE stable 0.1.0 (the whole point of R3-P2-1), never 0.0.0.
    expect(v.startsWith("0.0.0")).toBe(false);
  });

  it("R3-P2-1: dev without --version or --run-number/--dev-sha is rejected (no implicit 0.0.0)", async () => {
    await expect(
      mod.resolveVersion("dev", {}, async () => "9.9.9"),
    ).rejects.toThrow(/dev. channel requires --version/);
  });

  it("rejects an invalid --version", async () => {
    await expect(
      mod.resolveVersion("stable", { version: "not-a-version" }, async () => "0.1.0"),
    ).rejects.toThrow(/invalid --version/);
  });

  it("the built-in self-check passes (shape + path layout)", async () => {
    await expect(mod.selfCheck()).resolves.toBe(true);
  });

  it("splits a combined platform key into os/arch path segments", () => {
    expect(mod.osArchForTarget("windows-x86_64")).toEqual({
      os: "windows",
      arch: "x86_64",
    });
    expect(mod.osArchForTarget("darwin-aarch64")).toEqual({
      os: "darwin",
      arch: "aarch64",
    });
    expect(mod.osArchForTarget("linux-x86_64")).toEqual({
      os: "linux",
      arch: "x86_64",
    });
    expect(() => mod.osArchForTarget("garbage")).toThrow();
  });

  it("writes the manifest at the version-less os/arch path with real notes", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-vitest-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    // One fake signed Windows bundle.
    await fs.writeFile(path.join(bundles, "Driven_0.2.0_x64-setup.exe"), "fake");
    await fs.writeFile(
      path.join(bundles, "Driven_0.2.0_x64-setup.exe.sig"),
      "TESTSIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    const result = await mod.generate(
      "stable",
      {
        version: "0.2.0",
        bundles,
        out,
        baseUrl: "https://dl.example.test",
        notes: "## 0.2.0\n\n- Faster sync",
      },
      { readConfVersion: async () => "0.2.0", log: silent },
    );

    expect(result.version).toBe("0.2.0");
    expect(result.written.length).toBe(1);

    // R1-P1-1: the path is updates/stable/windows/x86_64/update.json - NO
    // version segment.
    const manifestPath = mod.manifestOutPath(out, "stable", "windows-x86_64");
    expect(manifestPath.replace(/\\/g, "/")).toContain(
      "stable/windows/x86_64/update.json",
    );
    expect(manifestPath).not.toContain("0.2.0");

    const manifest = JSON.parse(await fs.readFile(manifestPath, "utf8"));
    expect(manifest.version).toBe("0.2.0");
    expect(manifest.platforms["windows-x86_64"].signature).toBe("TESTSIG==");
    expect(manifest.platforms["windows-x86_64"].url).toBe(
      "https://dl.example.test/Driven_0.2.0_x64-setup.exe",
    );
    // R1-P1-6: the notes propagate into the manifest (the in-app changelog).
    expect(manifest.notes).toBe("## 0.2.0\n\n- Faster sync");
    expect(typeof manifest.pub_date).toBe("string");

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("reads notes from --notes-file and uses the rolling dev base URL", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-dev-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    const notesFile = path.join(tmp, "notes.md");
    await fs.mkdir(bundles, { recursive: true });
    await fs.writeFile(notesFile, "dev rolling notes\n");

    // A fake signed macOS bundle.
    await fs.writeFile(path.join(bundles, "Driven_aarch64.app.tar.gz"), "fake");
    await fs.writeFile(
      path.join(bundles, "Driven_aarch64.app.tar.gz.sig"),
      "DEVSIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    const result = await mod.generate(
      "dev",
      { version: "0.1.1-dev.123.abc1234", bundles, out, notesFile },
      { log: silent },
    );

    expect(result.version).toBe("0.1.1-dev.123.abc1234");
    const manifestPath = mod.manifestOutPath(out, "dev", "darwin-aarch64");
    expect(manifestPath.replace(/\\/g, "/")).toContain(
      "dev/darwin/aarch64/update.json",
    );
    const manifest = JSON.parse(await fs.readFile(manifestPath, "utf8"));
    // Notes came from the file (trimmed).
    expect(manifest.notes).toBe("dev rolling notes");
    // The dev base URL defaults to the rolling `dev` tag, NOT v<version>.
    expect(manifest.platforms["darwin-aarch64"].url).toBe(
      "https://github.com/pmaxhogan/driven/releases/download/dev/Driven_aarch64.app.tar.gz",
    );

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("rejects an unknown channel", async () => {
    await expect(
      mod.generate("nightly", {}, { readConfVersion: async () => "0.1.0" }),
    ).rejects.toThrow(/channel/);
  });

  it("parses a SemVer version token out of a bundle filename (R2-P1-2)", () => {
    expect(mod.versionFromBundleName("Driven_0.1.0_x64-setup.exe")).toBe("0.1.0");
    expect(
      mod.versionFromBundleName("Driven_0.1.1-dev.5.abc1234_x64-setup.exe"),
    ).toBe("0.1.1-dev.5.abc1234");
    // A version-less macOS bundle yields null.
    expect(mod.versionFromBundleName("Driven_aarch64.app.tar.gz")).toBeNull();
  });

  it("R2-P1-2: ERRORS on a stale accreted bundle (old version) instead of silently keeping it", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-stale-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    // Two Windows NSIS bundles for the SAME target but DIFFERENT versions: the
    // current run's 0.1.2 plus a stale 0.1.1 left over on the rolling release.
    await fs.writeFile(path.join(bundles, "Driven_0.1.2_x64-setup.exe"), "new");
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.2_x64-setup.exe.sig"),
      "NEWSIG==\n",
    );
    await fs.writeFile(path.join(bundles, "Driven_0.1.1_x64-setup.exe"), "old");
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.1_x64-setup.exe.sig"),
      "OLDSIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    // With the expected version armed, the stale 0.1.1 asset must abort the run -
    // NOT be silently published under 0.1.2.
    await expect(
      mod.generate(
        "stable",
        { version: "0.1.2", bundles, out, baseUrl: "https://dl.example.test" },
        { readConfVersion: async () => "0.1.2", log: silent },
      ),
    ).rejects.toThrow(/stale bundle|conflicting bundles/);

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R2-P1-2: keeps NSIS deterministically for the legitimate same-version msi+nsis pair", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-pair-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    // ONE Windows build emits both .msi and NSIS .exe, same version - a real
    // duplicate target. The generator keeps exactly one (NSIS) without erroring.
    await fs.writeFile(path.join(bundles, "Driven_0.2.0_x64.msi"), "msi");
    await fs.writeFile(path.join(bundles, "Driven_0.2.0_x64.msi.sig"), "MSISIG==\n");
    await fs.writeFile(path.join(bundles, "Driven_0.2.0_x64-setup.exe"), "exe");
    await fs.writeFile(
      path.join(bundles, "Driven_0.2.0_x64-setup.exe.sig"),
      "EXESIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    const result = await mod.generate(
      "stable",
      { version: "0.2.0", bundles, out, baseUrl: "https://dl.example.test" },
      { readConfVersion: async () => "0.2.0", log: silent },
    );

    // Exactly ONE manifest for windows-x86_64, pointing at the NSIS installer.
    expect(result.written.length).toBe(1);
    const manifestPath = mod.manifestOutPath(out, "stable", "windows-x86_64");
    const manifest = JSON.parse(await fs.readFile(manifestPath, "utf8"));
    expect(manifest.platforms["windows-x86_64"].signature).toBe("EXESIG==");
    expect(manifest.platforms["windows-x86_64"].url).toContain("-setup.exe");

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R3-P1-1: two arch-named mac bundles yield BOTH darwin/x86_64 AND darwin/aarch64 manifests", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-macduo-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    // Both mac jobs' arch-stamped updater artifacts (the contract release.yml +
    // dev-channel.yml now enforce).
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_darwin_aarch64.app.tar.gz"),
      "arm",
    );
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_darwin_aarch64.app.tar.gz.sig"),
      "ARMSIG==\n",
    );
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_darwin_x86_64.app.tar.gz"),
      "intel",
    );
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_darwin_x86_64.app.tar.gz.sig"),
      "INTELSIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    const result = await mod.generate(
      "stable",
      { version: "0.1.0", bundles, out, baseUrl: "https://dl.example.test" },
      { readConfVersion: async () => "0.1.0", log: silent },
    );

    expect(result.written.length).toBe(2);
    const arm = JSON.parse(
      await fs.readFile(mod.manifestOutPath(out, "stable", "darwin-aarch64"), "utf8"),
    );
    const intel = JSON.parse(
      await fs.readFile(mod.manifestOutPath(out, "stable", "darwin-x86_64"), "utf8"),
    );
    expect(arm.platforms["darwin-aarch64"].signature).toBe("ARMSIG==");
    expect(intel.platforms["darwin-x86_64"].signature).toBe("INTELSIG==");
    // The two arches must NOT collide onto one manifest.
    expect(arm.platforms["darwin-x86_64"]).toBeUndefined();
    expect(intel.platforms["darwin-aarch64"]).toBeUndefined();

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R3-P1-1: an archless mac bundle aborts the whole generate run", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-macbare-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });
    await fs.writeFile(path.join(bundles, "Driven.app.tar.gz"), "bare");
    await fs.writeFile(path.join(bundles, "Driven.app.tar.gz.sig"), "BARESIG==\n");

    const silent = { info: () => {}, warn: () => {} };
    await expect(
      mod.generate(
        "stable",
        { version: "0.1.0", bundles, out, baseUrl: "https://dl.example.test" },
        { readConfVersion: async () => "0.1.0", log: silent },
      ),
    ).rejects.toThrow(/archless macOS/);

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R3-P1-2: assertRequiredTargets passes for the full set, fails for a missing one", () => {
    const required = mod.V1_REQUIRED_TARGETS;
    expect(required).toEqual([
      "windows-x86_64",
      "darwin-x86_64",
      "darwin-aarch64",
      "linux-x86_64",
    ]);
    // Full set: no throw.
    expect(() => mod.assertRequiredTargets(required, [...required])).not.toThrow();
    // Missing darwin-aarch64: throws naming the gap.
    expect(() =>
      mod.assertRequiredTargets(required, [
        "windows-x86_64",
        "darwin-x86_64",
        "linux-x86_64",
      ]),
    ).toThrow(/darwin-aarch64/);
  });

  it("parseRequiredTargets splits + validates, rejects empty and malformed", () => {
    expect(mod.parseRequiredTargets("windows-x86_64, darwin-aarch64")).toEqual([
      "windows-x86_64",
      "darwin-aarch64",
    ]);
    // Dedupes.
    expect(
      mod.parseRequiredTargets("linux-x86_64 linux-x86_64"),
    ).toEqual(["linux-x86_64"]);
    expect(() => mod.parseRequiredTargets("   ")).toThrow(/no targets named/);
    expect(() => mod.parseRequiredTargets("garbage")).toThrow();
  });

  it("R3-P1-2: --require-targets makes generate FAIL when a required target is missing", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-partial-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    // Only a Windows bundle present - the mac + linux targets are MISSING, so a
    // require-targets run must refuse to publish this partial tree.
    await fs.writeFile(path.join(bundles, "Driven_0.1.0_x64-setup.exe"), "win");
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_x64-setup.exe.sig"),
      "WINSIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    await expect(
      mod.generate(
        "stable",
        {
          version: "0.1.0",
          bundles,
          out,
          baseUrl: "https://dl.example.test",
          requireTargets: "windows-x86_64,darwin-x86_64,darwin-aarch64,linux-x86_64",
        },
        { readConfVersion: async () => "0.1.0", log: silent },
      ),
    ).rejects.toThrow(/incomplete updater target set|missing/);

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R6-P1-2: an orphan .sig (no sibling installer) ERRORS instead of emitting a manifest", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-orphan-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    // A signature with NO sibling installer - a partial release. The generator
    // must NOT emit a valid-looking manifest whose download URL 404s.
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_x64-setup.exe.sig"),
      "ORPHANSIG==\n",
    );

    const silent = { info: () => {}, warn: () => {} };
    await expect(
      mod.generate(
        "stable",
        { version: "0.1.0", bundles, out, baseUrl: "https://dl.example.test" },
        { readConfVersion: async () => "0.1.0", log: silent },
      ),
    ).rejects.toThrow(/orphan signature|no sibling installer/);

    // No manifest was written for the orphan target.
    await expect(
      fs.stat(mod.manifestOutPath(out, "stable", "windows-x86_64")),
    ).rejects.toThrow();

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R6-P1-2: collectSignedBundles accepts a .sig only when the sibling bundle is a real file", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-orphan2-"));
    const bundles = path.join(tmp, "bundle");
    await fs.mkdir(bundles, { recursive: true });

    // Orphan: signature present, bundle absent -> rejected.
    await fs.writeFile(
      path.join(bundles, "Driven_0.1.0_x64-setup.exe.sig"),
      "SIG==\n",
    );
    const silent = { info: () => {}, warn: () => {} };
    await expect(
      mod.collectSignedBundles(bundles, silent, "0.1.0"),
    ).rejects.toThrow(/orphan signature|no sibling installer/);

    // Now drop the real installer next to it -> accepted, exactly one candidate.
    await fs.writeFile(path.join(bundles, "Driven_0.1.0_x64-setup.exe"), "real");
    const got = await mod.collectSignedBundles(bundles, silent, "0.1.0");
    expect(got.length).toBe(1);
    expect(got[0].target).toBe("windows-x86_64");

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("R3-P1-2: --require-targets passes when every required target is present", async () => {
    const tmp = await fs.mkdtemp(path.join(os.tmpdir(), "driven-updjson-full-"));
    const bundles = path.join(tmp, "bundle");
    const out = path.join(tmp, "out");
    await fs.mkdir(bundles, { recursive: true });

    const mk = async (name: string, sig: string) => {
      await fs.writeFile(path.join(bundles, name), "x");
      await fs.writeFile(path.join(bundles, `${name}.sig`), `${sig}\n`);
    };
    await mk("Driven_0.1.0_x64-setup.exe", "WIN==");
    await mk("Driven_0.1.0_darwin_x86_64.app.tar.gz", "MACX==");
    await mk("Driven_0.1.0_darwin_aarch64.app.tar.gz", "MACA==");
    await mk("driven_0.1.0_amd64.AppImage", "LIN==");

    const silent = { info: () => {}, warn: () => {} };
    const result = await mod.generate(
      "stable",
      {
        version: "0.1.0",
        bundles,
        out,
        baseUrl: "https://dl.example.test",
        requireTargets: "windows-x86_64,darwin-x86_64,darwin-aarch64,linux-x86_64",
      },
      { readConfVersion: async () => "0.1.0", log: silent },
    );
    expect(result.written.length).toBe(4);

    await fs.rm(tmp, { recursive: true, force: true });
  });
});
