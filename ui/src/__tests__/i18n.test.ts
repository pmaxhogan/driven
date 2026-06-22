import { describe, it, expect } from "vitest";
import { i18n } from "../i18n";

describe("i18n setup", () => {
  it("loads en-US and exposes the welcome string via t()", () => {
    const t = i18n.global.t;
    expect(t("app.welcome")).toBe("Driven");
    expect(t("app.tagline")).toMatch(/Google Drive/);
  });
});
