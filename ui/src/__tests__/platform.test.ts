import { describe, it, expect } from "vitest";

import { isMacUserAgent } from "../platform";

// Host-OS detection for the macOS updater gating (ROADMAP M9 R1-P2-1).

describe("isMacUserAgent", () => {
  it("matches macOS user-agents", () => {
    expect(
      isMacUserAgent(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
      ),
    ).toBe(true);
    expect(isMacUserAgent("something Mac OS X something")).toBe(true);
  });

  it("does not match Windows / Linux user-agents", () => {
    expect(
      isMacUserAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"),
    ).toBe(false);
    expect(
      isMacUserAgent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"),
    ).toBe(false);
    expect(isMacUserAgent("")).toBe(false);
  });
});
