import { describe, it, expect, vi, beforeEach } from "vitest";

// The typed IPC wrappers route every call through `@tauri-apps/api/core`'s
// `invoke`. That single import is the test seam: mocking it lets the wrappers
// (and the stores that use them) be unit-tested with no Tauri backend. This
// proves the seam works + pins the command-name + argument-shape contract the
// three M6 implementers code against.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import * as ipc from "../ipc/commands";

describe("typed IPC command wrappers", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
  });

  it("listAccounts invokes the right command name with no args", async () => {
    invokeMock.mockResolvedValueOnce([]);
    await ipc.listAccounts();
    expect(invokeMock).toHaveBeenCalledWith("list_accounts", undefined);
  });

  it("addSource passes the request under `req`", async () => {
    const req = {
      accountId: "a",
      displayName: "Docs",
      localPathToken: "tok-1",
      localPath: "/tmp/docs",
      driveFolderId: "f",
      driveFolderPath: "/Backups/Docs",
      encryptionEnabled: false,
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    };
    invokeMock.mockResolvedValueOnce({ source: { id: "s" }, recoveryPhrase: null });
    await ipc.addSource(req);
    expect(invokeMock).toHaveBeenCalledWith("add_source", { req });
  });

  it("syncNow forwards the camelCase argument name", async () => {
    await ipc.syncNow(null);
    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: null });
  });

  it("updateSettings forwards the patch under `patch`", async () => {
    const patch = { global: { skipOnBattery: false } };
    invokeMock.mockResolvedValueOnce({});
    await ipc.updateSettings(patch);
    expect(invokeMock).toHaveBeenCalledWith("update_settings", { patch });
  });

  it("submitOauthCredentials forwards session + credentials", async () => {
    await ipc.submitOauthCredentials("sess", "cid", "csecret");
    expect(invokeMock).toHaveBeenCalledWith("submit_oauth_credentials", {
      session: "sess",
      clientId: "cid",
      clientSecret: "csecret",
    });
  });
});
