import { describe, it, expect } from "vitest";
import path from "node:path";
import { fileURLToPath } from "node:url";

// set-dev-version.mjs pure-helper tests (ROADMAP M9 R1-P1-5). The dev-channel
// workflow patches 0.0.0-dev.<sha> into the canonical version sources before the
// Tauri build; these assert the workspace-version + JSON-version edits + the
// version validation without touching the real files.

const __filename = fileURLToPath(import.meta.url);
const SCRIPT = path.resolve(
  path.dirname(__filename),
  "../../../scripts/set-dev-version.mjs",
);

const mod = await import(SCRIPT);

const SAMPLE_CARGO = `[workspace]
members = ["a", "b"]

[workspace.package]
version = "0.1.0"
edition = "2021"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1" }
`;

describe("set-dev-version.mjs", () => {
  it("validates dev/prerelease + plain semver, rejects junk", () => {
    expect(mod.isValidVersion("0.1.0")).toBe(true);
    expect(mod.isValidVersion("0.0.0-dev.abc1234")).toBe(true);
    expect(mod.isValidVersion("1.2.3-rc.1+build.5")).toBe(true);
    expect(mod.isValidVersion("not-a-version")).toBe(false);
    expect(mod.isValidVersion("1.2")).toBe(false);
    expect(mod.isValidVersion("")).toBe(false);
  });

  it("replaces ONLY the [workspace.package] version, not dependency versions", () => {
    const out = mod.setWorkspaceVersion(SAMPLE_CARGO, "0.0.0-dev.abc1234");
    expect(out).toContain('version = "0.0.0-dev.abc1234"');
    // The dependency `version = "1"` lines are untouched.
    expect(out).toContain('tokio = { version = "1"');
    expect(out).toContain('serde = { version = "1" }');
    // Exactly one bare top-level `version = "..."` was rewritten.
    const bumped = out
      .split("\n")
      .filter((l: string) => /^version\s*=\s*"0\.0\.0-dev\.abc1234"/.test(l));
    expect(bumped.length).toBe(1);
  });

  it("throws when there is no [workspace.package] version", () => {
    expect(() =>
      mod.setWorkspaceVersion("[workspace]\nmembers = []\n", "0.2.0"),
    ).toThrow(/workspace.package/);
  });

  it("sets the top-level version on a JSON object", () => {
    expect(mod.setJsonVersion({ version: "0.1.0", name: "x" }, "0.2.0")).toEqual(
      { version: "0.2.0", name: "x" },
    );
  });

  it("reads the [workspace.package] version out of Cargo.toml (R2-P1-1)", () => {
    expect(mod.readWorkspaceVersion(SAMPLE_CARGO)).toBe("0.1.0");
    expect(() =>
      mod.readWorkspaceVersion("[workspace]\nmembers = []\n"),
    ).toThrow(/workspace.package/);
  });

  // A minimal SemVer comparator (SemVer 2.0.0 sec 11) sufficient to PROVE the
  // dev-version ordering claims: numeric prerelease identifiers compare
  // numerically; a version WITHOUT a prerelease outranks one WITH (same
  // major.minor.patch). Returns -1 / 0 / 1.
  function cmpSemver(a: string, b: string): number {
    const split = (v: string) => {
      const [core, pre] = v.split("-", 2);
      const nums = core.split(".").map((n) => Number(n));
      return { nums, pre: pre ?? null };
    };
    const A = split(a);
    const B = split(b);
    for (let i = 0; i < 3; i++) {
      if (A.nums[i] !== B.nums[i]) return A.nums[i] < B.nums[i] ? -1 : 1;
    }
    if (A.pre === B.pre) return 0;
    if (A.pre === null) return 1; // release > prerelease
    if (B.pre === null) return -1;
    const ai = A.pre.split(".");
    const bi = B.pre.split(".");
    for (let i = 0; i < Math.max(ai.length, bi.length); i++) {
      if (ai[i] === undefined) return -1;
      if (bi[i] === undefined) return 1;
      const an = /^\d+$/.test(ai[i]);
      const bn = /^\d+$/.test(bi[i]);
      if (an && bn) {
        const d = Number(ai[i]) - Number(bi[i]);
        if (d !== 0) return d < 0 ? -1 : 1;
      } else if (ai[i] !== bi[i]) {
        return ai[i] < bi[i] ? -1 : 1;
      }
    }
    return 0;
  }

  it("computeDevVersion derives <next-patch>-dev.<run>.<sha> from the stable version", () => {
    expect(mod.computeDevVersion("0.1.0", 123, "abc1234")).toBe(
      "0.1.1-dev.123.abc1234",
    );
    // Tracks future stable bumps (NOT hardcoded to 0.1.x).
    expect(mod.computeDevVersion("1.4.9", 7, "deadbee")).toBe(
      "1.4.10-dev.7.deadbee",
    );
    // run_number may arrive as a string (GITHUB_OUTPUT capture).
    expect(mod.computeDevVersion("0.1.0", "88", "f00")).toBe(
      "0.1.1-dev.88.f00",
    );
  });

  it("R2-P1-1: the dev version is ABOVE the current stable release", () => {
    const stable = "0.1.0";
    const dev = mod.computeDevVersion(stable, 1, "abc1234");
    expect(cmpSemver(dev, stable)).toBe(1);
    // And still below the NEXT stable release (so a real 0.1.1 supersedes it).
    expect(cmpSemver(dev, "0.1.1")).toBe(-1);
  });

  it("R2-P1-1: successive dev builds are MONOTONIC via run_number", () => {
    const v123 = mod.computeDevVersion("0.1.0", 123, "aaaaaaa");
    const v124 = mod.computeDevVersion("0.1.0", 124, "bbbbbbb");
    // run_number drives ordering even though the sha differs (sha is not the
    // sort key); 124 > 123 -> the later build is strictly greater.
    expect(cmpSemver(v124, v123)).toBe(1);
    // Sanity: the sha alone never inverts the order.
    const v123z = mod.computeDevVersion("0.1.0", 123, "zzzzzzz");
    expect(cmpSemver(v123z, v123)).toBe(1); // same run, sha tiebreak (z > a)
    expect(cmpSemver(v124, v123z)).toBe(1); // run 124 still beats run 123
  });

  it("computeDevVersion rejects a non-release base or bad run/sha", () => {
    expect(() => mod.computeDevVersion("0.1.0-dev.1.abc", 1, "abc")).toThrow(
      /clean/,
    );
    expect(() => mod.computeDevVersion("0.1.0", "1.2", "abc")).toThrow(
      /run_number/,
    );
    expect(() => mod.computeDevVersion("0.1.0", 1, "bad sha")).toThrow(/sha/);
  });

  it("the computed dev version is a valid SemVer the patcher accepts", () => {
    const dev = mod.computeDevVersion("0.1.0", 42, "cafe123");
    expect(mod.isValidVersion(dev)).toBe(true);
  });
});
