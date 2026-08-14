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

  it("has copy for the stale-dialog-token error code", () => {
    // `internal.stale_dialog_token` is what an expired/spent folder-pick token
    // rejects with (it used to be local.io_error, which told the user their
    // DISK was failing). An unknown code renders as the raw key, so a missing
    // bundle entry would silently regress the fix - pin that both strings
    // resolve and that the long copy points at the actual remedy.
    const t = i18n.global.t;
    expect(t("errors.internal.stale_dialog_token.short")).not.toContain("stale_dialog_token");
    expect(t("errors.internal.stale_dialog_token.long")).toMatch(/pick the folder again/i);
  });
});
