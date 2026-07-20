import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Store CRUD tests for the M6 settings/accounts/sources stores. The single test
// seam is `@tauri-apps/api/core`'s `invoke` (the typed IPC wrappers all route
// through it); mocking it lets us drive each store's CRUD against a fake backend
// and assert both the command name + argument shape AND the resulting store
// state. `@tauri-apps/api/event`'s `listen` is mocked too so the stores import
// cleanly (the accounts store's banner helper is exercised directly).

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));

import { useAccountsStore } from "../stores/accounts";
import { useSourcesStore } from "../stores/sources";
import { useSettingsStore } from "../stores/settings";
import type { AccountDto, SettingsDto, SourceDto } from "../ipc/types";

function makeAccount(over: Partial<AccountDto> = {}): AccountDto {
  return {
    id: "acc-1",
    email: "user@example.com",
    displayName: null,
    state: "ok",
    encryptionEnabled: false,
    createdAt: 0,
    lastSyncedAt: null,
    ...over,
  };
}

function makeSource(over: Partial<SourceDto> = {}): SourceDto {
  return {
    id: "src-1",
    accountId: "acc-1",
    displayName: "Docs",
    enabled: true,
    localPath: "/home/u/docs",
    driveFolderId: "f-1",
    driveFolderPath: "Backups/Docs",
    encryptionEnabled: false,
    respectGitignore: true,
    includePatterns: [],
    excludePatterns: [],
    placeholderPolicy: "skip",
    deepVerifyIntervalSecs: 604800,
    lastFullScanAt: null,
    createdAt: 0,
    pendingRecoveryAck: false,
    ...over,
  };
}

function makeSettings(over: Partial<SettingsDto> = {}): SettingsDto {
  return {
    global: {
      autoStartOnLogin: false,
      defaultConcurrentUploads: null,
      bandwidthCapMbps: null,
      skipOnBattery: true,
      skipOnMetered: true,
      scanIntervalSecs: 600,
      deepVerifyIntervalSecs: 604800,
      ioPriority: "low",
      logLevel: "info",
      schedule: {
        enabled: false,
        startMinute: 0,
        endMinute: 0,
        days: [true, true, true, true, true, true, true],
        utcOffsetMinutes: 0,
      },
      preBackupHook: null,
      postBackupHook: null,
      hookTimeoutSecs: 60,
      meteredMode: "pause",
      meteredBandwidthCapMbps: null,
    },
    telemetry: {
      enabled: true,
      installId: "id",
      endpoint: "https://example.test/ping",
    },
    updater: { channel: "stable", checkIntervalSecs: 21600 },
    ui: { trayLeftClickOpens: "activity", locale: "en-US", colorMode: "system" },
    windows: { vssMode: "auto", vssHelper: false },
    bundleSmallFiles: false,
    ...over,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

describe("accounts store", () => {
  it("refresh loads accounts via list_accounts", async () => {
    invokeMock.mockResolvedValueOnce([makeAccount()]);
    const store = useAccountsStore();
    await store.refresh();
    expect(invokeMock).toHaveBeenCalledWith("list_accounts", undefined);
    expect(store.accounts).toHaveLength(1);
    expect(store.loading).toBe(false);
    expect(store.error).toBeNull();
  });

  it("refresh captures the error string and resets loading", async () => {
    invokeMock.mockRejectedValueOnce(new Error("boom"));
    const store = useAccountsStore();
    await store.refresh();
    expect(store.error).toContain("boom");
    expect(store.loading).toBe(false);
  });

  it("remove forwards delete_remote and re-lists", async () => {
    const store = useAccountsStore();
    invokeMock.mockResolvedValueOnce(undefined); // remove_account
    invokeMock.mockResolvedValueOnce([]); // refresh
    await store.remove("acc-1", true);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "remove_account", {
      accountId: "acc-1",
      deleteRemote: true,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "list_accounts", undefined);
  });

  it("reauth returns the session id + auth url (A3)", async () => {
    invokeMock.mockResolvedValueOnce({
      sessionId: "sess-reauth",
      authUrl: "https://accounts.google/x",
    });
    const store = useAccountsStore();
    const { sessionId, authUrl } = await store.reauth("acc-1");
    expect(invokeMock).toHaveBeenCalledWith("reauth_account", {
      accountId: "acc-1",
    });
    expect(sessionId).toBe("sess-reauth");
    expect(authUrl).toBe("https://accounts.google/x");
  });

  it("completeReauth finishes onto the existing account when oauth completed (A3)", async () => {
    invokeMock.mockReset();
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "poll_oauth_status":
          return Promise.resolve({ kind: "complete" });
        case "finish_add_account":
          return Promise.resolve({
            id: "acc-1",
            email: "user@example.com",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
          });
        case "list_accounts":
          return Promise.resolve([]);
        default:
          return Promise.resolve(undefined);
      }
    });
    const store = useAccountsStore();
    const done = await store.completeReauth("sess-reauth");
    expect(done).toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("finish_add_account", {
      session: "sess-reauth",
      displayName: null,
    });
  });

  it("markNeedsReauth flips state and feeds the needsReauth getter", async () => {
    invokeMock.mockResolvedValueOnce([makeAccount({ id: "acc-1" })]);
    const store = useAccountsStore();
    await store.refresh();
    expect(store.needsReauth).toHaveLength(0);
    store.markNeedsReauth("acc-1");
    expect(store.accounts[0].state).toBe("needs_reauth");
    expect(store.needsReauth).toHaveLength(1);
    // Idempotent + ignores unknown ids.
    store.markNeedsReauth("acc-1");
    store.markNeedsReauth("nope");
    expect(store.needsReauth).toHaveLength(1);
  });
});

describe("sources store", () => {
  it("refresh loads sources via list_sources", async () => {
    invokeMock.mockResolvedValueOnce([makeSource()]);
    const store = useSourcesStore();
    await store.refresh();
    expect(invokeMock).toHaveBeenCalledWith("list_sources", undefined);
    expect(store.sources).toHaveLength(1);
  });

  it("add posts the request under `req` and re-lists", async () => {
    const store = useSourcesStore();
    const created = makeSource({ id: "src-new" });
    invokeMock.mockResolvedValueOnce({ source: created, recoveryPhrase: null }); // add_source
    invokeMock.mockResolvedValueOnce([created]); // refresh
    const req = {
      accountId: "acc-1",
      displayName: "Docs",
      localPathToken: "tok-1",
      localPath: "/home/u/docs",
      driveFolderId: "f-1",
      driveFolderPath: "Backups/Docs",
      encryptionEnabled: false,
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    };
    const result = await store.add(req);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "add_source", { req });
    expect(result.source.id).toBe("src-new");
    expect(store.sources).toHaveLength(1);
  });

  it("update patches a source and re-lists", async () => {
    const store = useSourcesStore();
    const updated = makeSource({ enabled: false });
    invokeMock.mockResolvedValueOnce(updated); // update_source
    invokeMock.mockResolvedValueOnce([updated]); // refresh
    const result = await store.update("src-1", { enabled: false });
    expect(invokeMock).toHaveBeenNthCalledWith(1, "update_source", {
      sourceId: "src-1",
      patch: { enabled: false },
    });
    expect(result.enabled).toBe(false);
  });

  it("remove forwards delete_remote and re-lists", async () => {
    const store = useSourcesStore();
    invokeMock.mockResolvedValueOnce(undefined); // remove_source
    invokeMock.mockResolvedValueOnce([]); // refresh
    await store.remove("src-1", false);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "remove_source", {
      sourceId: "src-1",
      deleteRemote: false,
    });
  });

  it("syncNow forwards the source id", async () => {
    const store = useSourcesStore();
    await store.syncNow("src-1");
    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: "src-1" });
  });
});

describe("settings store", () => {
  it("refresh loads settings via get_settings", async () => {
    invokeMock.mockResolvedValueOnce(makeSettings());
    const store = useSettingsStore();
    await store.refresh();
    expect(invokeMock).toHaveBeenCalledWith("get_settings", undefined);
    expect(store.settings?.global.scanIntervalSecs).toBe(600);
  });

  it("patch round-trips through update_settings and replaces the snapshot", async () => {
    const store = useSettingsStore();
    invokeMock.mockResolvedValueOnce(
      makeSettings({
        global: { ...makeSettings().global, skipOnBattery: false },
      })
    );
    await store.patch({ global: { skipOnBattery: false } });
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { skipOnBattery: false } },
    });
    expect(store.settings?.global.skipOnBattery).toBe(false);
  });

  it("patch surfaces the stable error CODE (not String(e)) and rethrows", async () => {
    // Regression: the store must store the SPEC s24 code via toErrorCode, never
    // String(e) - a structured `{ code, message }` error stringifies to the
    // literal "[object Object]", which used to render straight into the Rules tab.
    const store = useSettingsStore();
    invokeMock.mockRejectedValueOnce({ code: "internal.invalid_input", message: "out of range" });
    await expect(store.patch({ updater: { channel: "dev" } })).rejects.toMatchObject({
      code: "internal.invalid_input",
    });
    expect(store.errorCode).toBe("internal.invalid_input");
  });

  it("patch falls back to internal.bug for a code-less error", async () => {
    const store = useSettingsStore();
    invokeMock.mockRejectedValueOnce(new Error("boom"));
    await expect(store.patch({ updater: { channel: "dev" } })).rejects.toThrow("boom");
    expect(store.errorCode).toBe("internal.bug");
  });

  it("setTelemetryEnabled calls set_telemetry_enabled and updates the snapshot (R2-P1-1)", async () => {
    // R2-P1-1: the telemetry toggle routes through the DEDICATED command (not the
    // generic update_settings patch) so the backend flips the in-flight cancel
    // flag immediately; the store then reflects the new value in its snapshot.
    const store = useSettingsStore();
    invokeMock.mockResolvedValueOnce(makeSettings()); // refresh
    await store.refresh();
    expect(store.settings?.telemetry.enabled).toBe(true);

    invokeMock.mockResolvedValueOnce(false); // set_telemetry_enabled
    await store.setTelemetryEnabled(false);
    expect(invokeMock).toHaveBeenCalledWith("set_telemetry_enabled", {
      enabled: false,
    });
    expect(store.settings?.telemetry.enabled).toBe(false);
    // It did NOT route through the generic update_settings patch.
    expect(invokeMock).not.toHaveBeenCalledWith("update_settings", {
      patch: { telemetry: { enabled: false } },
    });
  });

  it("setTelemetryEnabled surfaces the error code and rethrows", async () => {
    const store = useSettingsStore();
    invokeMock.mockRejectedValueOnce({ code: "internal.bug", message: "toggle failed" });
    await expect(store.setTelemetryEnabled(false)).rejects.toMatchObject({ code: "internal.bug" });
    expect(store.errorCode).toBe("internal.bug");
  });
});
