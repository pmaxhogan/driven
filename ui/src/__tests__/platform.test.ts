import { describe, it, expect, afterEach } from "vitest";

import { isMacUserAgent, isMacOS, isWindowsUserAgent, isWindows } from "../platform";

// Host-OS detection for the macOS updater gating (ROADMAP M9 R1-P2-1) and for
// hiding Windows-only controls (the OneDrive placeholder policy).

describe("isMacUserAgent", () => {
  it("matches macOS user-agents", () => {
    expect(
      isMacUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15")
    ).toBe(true);
    expect(isMacUserAgent("something Mac OS X something")).toBe(true);
  });

  it("does not match Windows / Linux user-agents", () => {
    expect(isMacUserAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")).toBe(false);
    expect(isMacUserAgent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")).toBe(false);
    expect(isMacUserAgent("")).toBe(false);
  });
});

describe("isWindowsUserAgent", () => {
  it("matches Windows user-agents", () => {
    expect(isWindowsUserAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")).toBe(true);
    // The WebView2 UA on Windows 11.
    expect(
      isWindowsUserAgent(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Edg/120.0.0.0"
      )
    ).toBe(true);
  });

  it("does not match macOS / Linux user-agents", () => {
    expect(
      isWindowsUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15")
    ).toBe(false);
    expect(isWindowsUserAgent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")).toBe(false);
    expect(isWindowsUserAgent("")).toBe(false);
  });

  it("is mutually exclusive with isMacUserAgent on real user-agents", () => {
    // A control gated on one must never also be gated on the other.
    const mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15";
    const win = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";
    expect([isMacUserAgent(mac), isWindowsUserAgent(mac)]).toEqual([true, false]);
    expect([isMacUserAgent(win), isWindowsUserAgent(win)]).toEqual([false, true]);
  });
});

describe("live-navigator wrappers", () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "navigator");

  function setUserAgent(ua: string | undefined): void {
    Object.defineProperty(globalThis, "navigator", {
      value: ua === undefined ? {} : { userAgent: ua },
      configurable: true,
      writable: true,
    });
  }

  afterEach(() => {
    if (original) {
      Object.defineProperty(globalThis, "navigator", original);
    } else {
      // @ts-expect-error - removing the stub we installed.
      delete globalThis.navigator;
    }
  });

  it("reads the live userAgent", () => {
    setUserAgent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)");
    expect(isWindows()).toBe(true);
    expect(isMacOS()).toBe(false);

    setUserAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)");
    expect(isWindows()).toBe(false);
    expect(isMacOS()).toBe(true);
  });

  it("falls back to false when there is no usable navigator", () => {
    // Neither wrapper may throw where navigator (or its userAgent) is absent -
    // that is what keeps them callable at module scope in a component.
    setUserAgent(undefined);
    expect(isWindows()).toBe(false);
    expect(isMacOS()).toBe(false);
  });
});
