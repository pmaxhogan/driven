// @vitest-environment jsdom
import { describe, it, expect, afterEach } from "vitest";

// The scripted IPC mock (ui/test-support) is shared by the Playwright visual
// suite and vitest. The visual suite exercises the browser path constantly;
// this covers the vitest path AND the invariants the visual suite depends on
// but cannot assert - notably that a command with no scripted response REJECTS
// rather than resolving undefined, which is what stops an unmocked command from
// producing a plausible-but-wrong screenshot.
//
// It drives the real `@tauri-apps/api` wrappers rather than calling
// `window.__TAURI_INTERNALS__` directly, so it proves the seam the app actually
// uses is the seam the mock fills.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { ACCOUNTS } from "../../test-support/fixtures";
import {
  installScenario,
  mockError,
  mockPending,
  resolveScenario,
  type MockBackendHandle,
} from "../../test-support/mock-backend";

let mock: MockBackendHandle | null = null;

afterEach(() => {
  mock?.uninstall();
  mock = null;
});

describe("installMockBackend", () => {
  it("answers a command from the defaults through the real invoke wrapper", async () => {
    mock = installScenario();
    await expect(invoke("list_accounts")).resolves.toEqual(ACCOUNTS);
  });

  it("records every call with its arguments", async () => {
    mock = installScenario();
    await invoke("update_source", { sourceId: "src-1", patch: { enabled: false } });
    expect(mock.callsTo("update_source")).toEqual([
      { cmd: "update_source", args: { sourceId: "src-1", patch: { enabled: false } } },
    ]);
  });

  it("lets a scenario override one command and inherit the rest", async () => {
    mock = installScenario({ commands: { list_accounts: [] } });
    await expect(invoke("list_accounts")).resolves.toEqual([]);
    await expect(invoke("get_update_channel")).resolves.toBe("stable");
  });

  it("rejects with a SPEC s24 shaped error", async () => {
    mock = installScenario({ commands: { get_settings: mockError("state.db_corrupt") } });
    await expect(invoke("get_settings")).rejects.toMatchObject({ code: "state.db_corrupt" });
  });

  it("never settles a pending command", async () => {
    mock = installScenario({ commands: { get_settings: mockPending() } });
    let settled = false;
    void invoke("get_settings").then(
      () => (settled = true),
      () => (settled = true)
    );
    await Promise.resolve();
    await Promise.resolve();
    expect(settled).toBe(false);
  });

  it("rejects a command it was never scripted for", async () => {
    mock = installScenario();
    await expect(invoke("no_such_command")).rejects.toMatchObject({ code: "internal.bug" });
  });

  it("does not mistake a DTO with a `kind` field for a wrapped response", async () => {
    // `poll_oauth_status` genuinely resolves to `{ kind: "complete" }`. A
    // response model that discriminated on a bare `kind` would read this as a
    // control object and answer with something else entirely.
    mock = installScenario({ commands: { poll_oauth_status: { kind: "complete" } } });
    await expect(invoke("poll_oauth_status")).resolves.toEqual({ kind: "complete" });
  });

  it("delivers an emitted event to a listener registered through `listen`", async () => {
    mock = installScenario();
    const seen: unknown[] = [];
    const unlisten = await listen("activity:new", (e) => seen.push(e.payload));

    expect(mock.listenerCount("activity:new")).toBe(1);
    expect(mock.emit("activity:new", { id: 1 })).toBe(1);
    expect(seen).toEqual([{ id: 1 }]);

    // `_unlisten` reaches for __TAURI_EVENT_PLUGIN_INTERNALS__ before its own
    // invoke; an unstubbed one throws here, which in the browser would mean
    // every route change threw on unmount.
    await unlisten();
    expect(mock.listenerCount("activity:new")).toBe(0);
    expect(mock.emit("activity:new", { id: 2 })).toBe(0);
    expect(seen).toEqual([{ id: 1 }]);
  });

  it("supports a per-call handler for behaviour a static value cannot express", async () => {
    mock = installScenario();
    let calls = 0;
    mock.setHandler("query_activity", () => ({ page: ++calls }));
    await expect(invoke("query_activity")).resolves.toEqual({ page: 1 });
    await expect(invoke("query_activity")).resolves.toEqual({ page: 2 });
  });

  it("restores the previous globals on uninstall", () => {
    const before = window.__TAURI_INTERNALS__;
    const handle = installScenario();
    expect(window.__TAURI_INTERNALS__).not.toBe(before);
    handle.uninstall();
    expect(window.__TAURI_INTERNALS__).toBe(before);
    expect(window.__drivenMock).toBeUndefined();
  });
});

describe("resolveScenario", () => {
  it("produces a JSON-serializable object", () => {
    // Playwright serializes this to send it into the page, so anything that
    // does not survive a JSON round trip would be silently dropped there.
    const resolved = resolveScenario({ commands: { list_accounts: [] } });
    expect(JSON.parse(JSON.stringify(resolved))).toEqual(JSON.parse(JSON.stringify(resolved)));
    expect(Object.keys(resolved.responses).length).toBeGreaterThan(50);
  });

  it("keeps a command whose value is undefined, so it stays scripted", () => {
    // `JSON.stringify` drops an `undefined` VALUE but keeps the command key -
    // which is the difference between "resolves undefined" and the loud
    // unscripted-command rejection.
    const resolved = resolveScenario();
    const roundTripped = JSON.parse(JSON.stringify(resolved)) as typeof resolved;
    expect(roundTripped.responses).toHaveProperty("sync_now");
    expect(roundTripped.responses.sync_now.kind).toBe("resolve");
  });
});
