import { describe, it, expect } from "vitest";
import { i18n } from "../i18n";

describe("i18n setup", () => {
  it("loads en-US and exposes the welcome string via t()", () => {
    const t = i18n.global.t;
    expect(t("app.welcome")).toBe("Driven");
    // Assert the key RESOLVES rather than pinning the wording: the tagline is
    // product copy that changes (it named Google Drive until destinations
    // became pluggable), and a test that pins marketing text just breaks on
    // every edit without checking that i18n works.
    expect(t("app.tagline")).toMatch(/backup/i);
  });
});
