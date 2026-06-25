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
});
