import { describe, it, expect } from "vitest";
import { promises as fs } from "node:fs";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";

// generate-update-json.mjs smoke test (SPEC s15 / s19.3; ROADMAP M9). Imports the
// pure functions from the script (no network, no real bundles) and asserts the
// emitted update.json SHAPE + PATH LAYOUT against a temp fixture of fake
// bundles+.sig - the exact contract the in-app updater's endpoint URL expects:
//   updates/<channel>/<target>/<version>/update.json
// with a `{ version, notes, pub_date, platforms: { <target>: { signature, url } } }`
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
    expect(mod.targetForBundle("driven_0.1.0_amd64.AppImage")).toBe(
      "linux-x86_64",
    );
    // A non-updater artifact maps to null (skipped).
    expect(mod.targetForBundle("Driven_0.1.0_amd64.deb")).toBeNull();
  });

  it("resolves the dev version from a sha", async () => {
    const v = await mod.resolveVersion("dev", { sha: "abc1234" }, async () => "9.9.9");
    expect(v).toBe("0.0.0-dev.abc1234");
  });

  it("resolves the stable version from the conf reader", async () => {
    const v = await mod.resolveVersion("stable", {}, async () => "0.1.0");
    expect(v).toBe("0.1.0");
  });

  it("the built-in self-check passes (shape + path layout)", async () => {
    await expect(mod.selfCheck()).resolves.toBe(true);
  });

  it("generates a per-target manifest at the endpoint-shaped path", async () => {
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
      { version: "0.2.0", bundles, out, baseUrl: "https://dl.example.test" },
      { readConfVersion: async () => "0.2.0", log: silent },
    );

    expect(result.version).toBe("0.2.0");
    expect(result.written.length).toBe(1);

    const manifestPath = mod.manifestOutPath(
      out,
      "stable",
      "windows-x86_64",
      "0.2.0",
    );
    const manifest = JSON.parse(await fs.readFile(manifestPath, "utf8"));
    expect(manifest.version).toBe("0.2.0");
    expect(manifest.platforms["windows-x86_64"].signature).toBe("TESTSIG==");
    expect(manifest.platforms["windows-x86_64"].url).toBe(
      "https://dl.example.test/Driven_0.2.0_x64-setup.exe",
    );
    expect(typeof manifest.pub_date).toBe("string");

    await fs.rm(tmp, { recursive: true, force: true });
  });

  it("rejects an unknown channel", async () => {
    await expect(
      mod.generate("nightly", {}, { readConfVersion: async () => "0.1.0" }),
    ).rejects.toThrow(/channel/);
  });
});
