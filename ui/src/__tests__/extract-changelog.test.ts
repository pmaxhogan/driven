import { describe, it, expect } from "vitest";
import path from "node:path";
import { fileURLToPath } from "node:url";

// extract-changelog.mjs tests (ROADMAP M9 R2-P2-2). The release pipeline extracts
// a tag's CHANGELOG.md section to feed DETERMINISTIC release notes to BOTH the
// GitHub Release body AND the updater manifest (the in-app "View changelog").
// These assert the pure extractor over the release-please / Keep-a-Changelog
// heading shapes - no file IO, no network.

const __filename = fileURLToPath(import.meta.url);
const SCRIPT = path.resolve(path.dirname(__filename), "../../../scripts/extract-changelog.mjs");

const mod = await import(SCRIPT);

// A representative release-please CHANGELOG.md.
const CHANGELOG = `# Changelog

## [Unreleased]

## [0.2.0](https://github.com/pmaxhogan/driven/compare/v0.1.0...v0.2.0) (2026-07-01)

### Features

- **sync:** faster incremental scan
- **ui:** dark mode

### Bug Fixes

- handle empty folders

## [0.1.0](https://github.com/pmaxhogan/driven/compare/v0.0.1...v0.1.0) (2026-06-01)

### Features

- initial GA release
`;

describe("extract-changelog.mjs", () => {
  it("normalizes a version token (strips a leading v)", () => {
    expect(mod.normalizeVersion("v0.2.0")).toBe("0.2.0");
    expect(mod.normalizeVersion("0.2.0")).toBe("0.2.0");
    expect(mod.normalizeVersion("  V1.2.3  ")).toBe("1.2.3");
  });

  it("pulls the version out of the various heading shapes", () => {
    expect(
      mod.headingVersion("## [0.2.0](https://github.com/o/r/compare/v0.1.0...v0.2.0) (2026-07-01)")
    ).toBe("0.2.0");
    expect(mod.headingVersion("## [0.1.0] - 2026-06-01")).toBe("0.1.0");
    expect(mod.headingVersion("## 0.3.0 (2026-08-01)")).toBe("0.3.0");
    expect(mod.headingVersion("## [Unreleased]")).toBe("Unreleased");
    // Non-heading lines yield null.
    expect(mod.headingVersion("- a bullet")).toBeNull();
    expect(mod.headingVersion("### Features")).toBeNull();
  });

  it("R2-P2-2: extracts a non-empty section for a tagged version (with or without v)", () => {
    const sectionV = mod.extractSection(CHANGELOG, "v0.2.0");
    const section = mod.extractSection(CHANGELOG, "0.2.0");
    expect(sectionV).toBe(section);
    expect(section.length).toBeGreaterThan(0);
    // Contains this release's content...
    expect(section).toContain("faster incremental scan");
    expect(section).toContain("handle empty folders");
    // ...and STOPS before the next version's section (no bleed-through).
    expect(section).not.toContain("initial GA release");
    // The section's OWN heading is excluded (it is the body only).
    expect(section).not.toContain("## [0.2.0]");
  });

  it("extracts the oldest section without trailing bleed", () => {
    const section = mod.extractSection(CHANGELOG, "0.1.0");
    expect(section).toContain("initial GA release");
    expect(section).not.toContain("faster incremental scan");
  });

  it("returns empty for an unknown / not-yet-released version", () => {
    expect(mod.extractSection(CHANGELOG, "9.9.9")).toBe("");
  });
});
