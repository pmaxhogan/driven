// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createMemoryHistory } from "vue-router";

// The first-run logic reads account presence through the SAME `list_accounts`
// IPC command AccountList.vue uses, which routes through `@tauri-apps/api/core`'s
// `invoke`. Mocking that single seam lets us drive the guard against a fake
// backend (zero accounts, some accounts, or an outright IPC failure) with no
// Tauri runtime - jsdom supplies the `window` that the singleton router's
// createWebHistory() needs at import time.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { firstRunTarget, createAppRouter } from "../router";

const ACCOUNT = {
  id: "acct-1",
  email: "user@example.com",
  displayName: null,
  state: "ok",
  encryptionEnabled: false,
  createdAt: 0,
  lastSyncedAt: null,
};

/** Route list_accounts to a fixed account list (default: empty = fresh install). */
function backendWithAccounts(accounts: unknown[]): void {
  invokeMock.mockImplementation((cmd: string) => {
    if (cmd === "list_accounts") return Promise.resolve(accounts);
    return Promise.resolve(undefined);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  backendWithAccounts([]);
});

describe("firstRunTarget (UI-CORE first-run decision)", () => {
  it("diverts the default landing to /setup when there are zero accounts", async () => {
    backendWithAccounts([]);
    expect(await firstRunTarget("/")).toBe("/setup");
    expect(await firstRunTarget("/activity")).toBe("/setup");
  });

  it("keeps the default landing when at least one account exists", async () => {
    backendWithAccounts([ACCOUNT]);
    expect(await firstRunTarget("/")).toBeNull();
    expect(await firstRunTarget("/activity")).toBeNull();
  });

  it("never diverts a deep-link to a specific surface (no trap)", async () => {
    backendWithAccounts([]); // zero accounts, yet a deep-link is honoured
    expect(await firstRunTarget("/accounts")).toBeNull();
    expect(await firstRunTarget("/restore")).toBeNull();
    expect(await firstRunTarget("/about")).toBeNull();
    expect(await firstRunTarget("/setup")).toBeNull();
  });

  it("falls through to the normal landing on an IPC failure (never crashes)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.reject(new Error("backend down"));
      return Promise.resolve(undefined);
    });
    await expect(firstRunTarget("/")).resolves.toBeNull();
    await expect(firstRunTarget("/activity")).resolves.toBeNull();
  });
});

describe("first-run navigation guard (createAppRouter)", () => {
  it("lands a fresh install (zero accounts) on the setup wizard", async () => {
    backendWithAccounts([]);
    const router = createAppRouter(createMemoryHistory());
    await router.push("/");
    await router.isReady();
    expect(router.currentRoute.value.path).toBe("/setup");
  });

  it("lands a configured install on /activity", async () => {
    backendWithAccounts([ACCOUNT]);
    const router = createAppRouter(createMemoryHistory());
    await router.push("/");
    await router.isReady();
    expect(router.currentRoute.value.path).toBe("/activity");
  });

  it("honours a deep-link even with zero accounts", async () => {
    backendWithAccounts([]);
    const router = createAppRouter(createMemoryHistory());
    await router.push("/restore");
    await router.isReady();
    expect(router.currentRoute.value.path).toBe("/restore");
  });

  it("is one-shot - cannot re-trap the user after the first navigation", async () => {
    backendWithAccounts([]);
    const router = createAppRouter(createMemoryHistory());
    // First launch with zero accounts diverts to /setup...
    await router.push("/");
    await router.isReady();
    expect(router.currentRoute.value.path).toBe("/setup");
    // ...but navigating back to the default surface afterwards is NOT re-diverted,
    // even though there are still zero accounts (guard self-removed).
    await router.push("/activity");
    expect(router.currentRoute.value.path).toBe("/activity");
  });

  it("registers the /settings route rendering the accounts tab by default", async () => {
    backendWithAccounts([ACCOUNT]);
    const router = createAppRouter(createMemoryHistory());
    await router.push("/settings");
    await router.isReady();
    const matched = router.currentRoute.value.matched[0];
    expect(router.currentRoute.value.name).toBe("settings");
    expect(matched.props.default).toEqual({ tab: "accounts" });
  });
});
