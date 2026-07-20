// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import type { ScheduleSettings, SettingsDto, SourceDto } from "../ipc/types";

// Component tests for the M6 settings UI: the SourceTable row actions, the
// AddSourceWizard multi-step flow, and the Rules-tab round-trip. They drive the
// real components against a faked backend (the `invoke` seam) + a faked
// tauri-plugin-dialog (so the folder pickers resolve deterministically), and
// assert that the right IPC commands fire with the right argument shapes.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));
const openDialogMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openDialogMock(...args),
}));
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("0.1.0"),
}));

// vue-router is mocked: the components only need useRouter().push (AccountList /
// Settings) - we never exercise navigation here.
const pushMock = vi.fn();
vi.mock("vue-router", () => ({
  useRouter: () => ({ push: pushMock }),
  useRoute: () => ({ params: {} }),
}));

import SourceTable from "../components/SourceTable.vue";
import AddSourceWizard from "../components/AddSourceWizard.vue";
import Settings from "../views/Settings.vue";

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
      customRootCaPath: null,
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

const globalMountOptions = { plugins: [i18n] };

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  openDialogMock.mockReset();
  pushMock.mockReset();
});

describe("SourceTable", () => {
  it("renders a row per source with the resolved account email", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts")
        return Promise.resolve([
          {
            id: "acc-1",
            email: "user@example.com",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
          },
        ]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    expect(wrapper.text()).toContain("Docs");
    expect(wrapper.text()).toContain("user@example.com");
  });

  it("toggling the enabled checkbox patches the source", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "update_source") return Promise.resolve(makeSource({ enabled: false }));
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    const checkbox = wrapper.get('input[type="checkbox"]');
    await checkbox.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_source", {
      sourceId: "src-1",
      patch: { enabled: false },
    });
  });

  it("opens the versioning panel and saves the per-source config (issue #36)", async () => {
    let saved: unknown = null;
    invokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "get_source_versioning")
        return Promise.resolve({ enabled: false, countCap: 10, maxBytes: 0 });
      if (cmd === "set_source_versioning") {
        saved = args;
        return Promise.resolve({ enabled: true, countCap: 5, maxBytes: 0 });
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();

    // Open the panel (loads the current config).
    await wrapper.get('[data-testid="versioning-button"]').trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="versioning-editor"]').exists()).toBe(true);

    // Enable + set a keep-N cap, then save.
    await wrapper.get('[data-testid="versioning-enabled"]').setValue(true);
    await wrapper.get('[data-testid="versioning-cap"]').setValue(5);
    await wrapper.get('[data-testid="versioning-save"]').trigger("click");
    await flushPromises();

    expect(saved).toMatchObject({
      sourceId: "src-1",
      config: { enabled: true, countCap: 5, maxBytes: 0 },
    });
    // The panel closes after a successful save.
    expect(wrapper.find('[data-testid="versioning-editor"]').exists()).toBe(false);
  });

  it("shows an error instead of stale inputs when versioning config load fails (issue #36)", async () => {
    // Source A's config loads; source B's REJECTS. Opening B must NOT render the
    // editor over A's stale enabled/cap (Save would persist A's values to B) - it
    // must surface the error and hide both the inputs and Save.
    invokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "list_sources")
        return Promise.resolve([makeSource({ id: "src-a" }), makeSource({ id: "src-b" })]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "get_source_versioning") {
        const id = (args as { sourceId: string }).sourceId;
        return id === "src-a"
          ? Promise.resolve({ enabled: true, countCap: 3, maxBytes: 0 })
          : Promise.reject(new Error("transient db error"));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();

    const buttons = wrapper.findAll('[data-testid="versioning-button"]');
    expect(buttons.length).toBe(2);

    // Open A: its config loads and the inputs render (enabled=true, cap=3).
    await buttons[0].trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="versioning-enabled"]').exists()).toBe(true);

    // Open B: the load rejects. Only the error renders - no stale inputs, no Save.
    await buttons[1].trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="versioning-error"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="versioning-enabled"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="versioning-save"]').exists()).toBe(false);
  });

  it("disables the enable toggle for a pending-recovery-ack source (R4-P1-2)", async () => {
    // R4-P1-2 (DATA-SAFETY): a first-encrypted source still awaiting its recovery
    // phrase ack must not be enableable from the table - the toggle is disabled
    // (with a tooltip + badge) and a change is a no-op (no update_source call).
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([
          makeSource({ encryptionEnabled: true, enabled: false, pendingRecoveryAck: true }),
        ]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();

    const checkbox = wrapper.get('input[type="checkbox"]');
    expect((checkbox.element as HTMLInputElement).disabled).toBe(true);
    expect(wrapper.find('[data-testid="pending-recovery-ack-badge"]').exists()).toBe(true);

    // Even if a change event is fired, the handler is a no-op (no update_source).
    await checkbox.trigger("change");
    await flushPromises();
    expect(invokeMock).not.toHaveBeenCalledWith("update_source", expect.anything());
  });

  it("exposes a post-restart reveal/ack action that enables a pending source (R5-P1-2, R7-P2-1)", async () => {
    // R5-P1-2 (DATA-SAFETY): a first-encrypted source that survived a restart is
    // durably pending; the table must expose a reachable reveal/ack action.
    // R7-P2-1 (DATA-SAFETY): opening the panel must NOT record the backend reveal -
    // the reveal_recovery_phrase IPC fires only when the user clicks Reveal inside
    // RecoveryPhraseReveal. A successful ack (ack_recovery_phrase_saved) then enables
    // the source and clears the pending state - reachable WITHOUT the volatile wizard.
    const words = Array.from({ length: 24 }, (_, i) => `word${i + 1}`);
    let acked = false;
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([
          makeSource({
            encryptionEnabled: true,
            enabled: false,
            pendingRecoveryAck: !acked,
          }),
        ]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "reveal_recovery_phrase") return Promise.resolve(words);
      if (cmd === "ack_recovery_phrase_saved") {
        acked = true;
        return Promise.resolve(
          makeSource({ encryptionEnabled: true, enabled: true, pendingRecoveryAck: false })
        );
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();

    // Opening the panel must NOT record a reveal (R7-P2-1).
    const revealBtn = wrapper.get('[data-testid="reveal-ack-button"]');
    await revealBtn.trigger("click");
    await flushPromises();
    expect(invokeMock).not.toHaveBeenCalledWith("reveal_recovery_phrase", expect.anything());

    // The reveal/ack panel is open. Clicking Reveal inside RecoveryPhraseReveal is
    // what records the backend reveal AND fetches the words (so the ack checkbox
    // unlocks). The gate requires the user to actually click Reveal.
    const panel = wrapper.get('[data-testid="reveal-ack-panel"]');
    const showButton = panel
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    await showButton!.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("reveal_recovery_phrase", {
      sourceId: "src-1",
    });

    // Tick the acknowledgement checkbox, then confirm -> ack enables the source.
    await panel.get('[data-testid="phrase-ack"]').setValue(true);
    const confirm = panel.get('[data-testid="reveal-ack-confirm"]');
    expect((confirm.element as HTMLButtonElement).disabled).toBe(false);
    await confirm.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("ack_recovery_phrase_saved", {
      sourceId: "src-1",
    });
    // The panel closed (pending state cleared after the refresh).
    expect(wrapper.find('[data-testid="reveal-ack-panel"]').exists()).toBe(false);
  });

  it("opening + cancelling the reveal panel never records a backend reveal (R7-P2-1)", async () => {
    // R7-P2-1 (DATA-SAFETY): the durable revealed=1 state may only be set after the
    // user clicks Reveal. Opening the panel then cancelling (without ever clicking
    // Reveal) must NOT call reveal_recovery_phrase - so a user who backs out never
    // weakens the "revealed == the user actually saw the phrase" invariant.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([
          makeSource({
            encryptionEnabled: true,
            enabled: false,
            pendingRecoveryAck: true,
          }),
        ]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();

    // Open the panel.
    await wrapper.get('[data-testid="reveal-ack-button"]').trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="reveal-ack-panel"]').exists()).toBe(true);

    // Cancel WITHOUT clicking Reveal.
    const panel = wrapper.get('[data-testid="reveal-ack-panel"]');
    const cancelButton = panel
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.cancel"));
    await cancelButton!.trigger("click");
    await flushPromises();

    // No backend reveal was recorded, and the panel is closed.
    expect(invokeMock).not.toHaveBeenCalledWith("reveal_recovery_phrase", expect.anything());
    expect(wrapper.find('[data-testid="reveal-ack-panel"]').exists()).toBe(false);
  });

  it("Run now fires sync_now for the row's source", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    const runNow = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.runNowButton"));
    expect(runNow).toBeTruthy();
    await runNow!.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: "src-1" });
  });

  it("remove confirmation forwards the delete-remote choice", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    const removeButton = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.removeButton"));
    await removeButton!.trigger("click");
    await flushPromises();
    const confirmPanel = wrapper.get('[data-testid="source-remove-confirm"]');
    await confirmPanel.get('input[type="checkbox"]').setValue(true);
    const confirmRemove = confirmPanel
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.removeButton"));
    await confirmRemove!.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("remove_source", {
      sourceId: "src-1",
      deleteRemote: true,
    });
  });

  it("Edit exclusions opens the inline editor and saves a patch", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions")
        return Promise.resolve({
          includedCount: 3,
          excludedCount: 1,
          includedBytes: 1024,
          includedSample: ["a", "b", "c"],
          excludedSample: ["d"],
          truncated: false,
        });
      if (cmd === "update_source") return Promise.resolve(makeSource());
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    const editButton = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.editExclusionsButton"));
    await editButton!.trigger("click");
    await flushPromises();
    const editor = wrapper.get('[data-testid="exclusion-editor"]');
    expect(invokeMock).toHaveBeenCalledWith(
      "preview_exclusions",
      // R1-P1-2: an EXISTING source is previewed by its id (the backend resolves
      // the local path from SQLite), NEVER a raw webview path. The wrapper nests
      // the request under `req` (matching the Rust signature).
      expect.objectContaining({
        req: expect.objectContaining({ sourceId: "src-1" }),
      })
    );
    const excludeArea = editor.findAll("textarea")[1];
    await excludeArea.setValue("node_modules\n*.log");
    const saveButton = editor
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.save"));
    await saveButton!.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_source", {
      sourceId: "src-1",
      patch: {
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: ["node_modules", "*.log"],
        // Issue #4: the edit patch always carries the placeholder policy; an
        // unchanged source (default "skip") sends "skip".
        placeholderPolicy: "skip",
      },
    });
  });

  it("issue #4: toggling the cloud-only backup checkbox patches placeholderPolicy", async () => {
    // The edit-exclusions panel exposes the OneDrive / cloud-only placeholder
    // toggle. It reflects the source's current policy ("skip" here) and, when the
    // user turns it on, the saved patch carries "force_download".
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions")
        return Promise.resolve({
          includedCount: 1,
          excludedCount: 0,
          includedBytes: 1,
          includedSample: ["a"],
          excludedSample: [],
          truncated: false,
        });
      if (cmd === "update_source")
        return Promise.resolve(makeSource({ placeholderPolicy: "force_download" }));
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    const editButton = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.editExclusionsButton"));
    await editButton!.trigger("click");
    await flushPromises();
    const editor = wrapper.get('[data-testid="exclusion-editor"]');

    // The toggle starts unchecked (source policy is the default "skip").
    const toggle = editor.get('[data-testid="placeholder-policy-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(false);

    // Turn it on and save: the patch carries force_download.
    await toggle.setValue(true);
    const saveButton = editor
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.save"));
    await saveButton!.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_source", {
      sourceId: "src-1",
      patch: expect.objectContaining({ placeholderPolicy: "force_download" }),
    });
  });

  it("issue #4: the edit toggle reflects an already-force_download source", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([makeSource({ placeholderPolicy: "force_download" })]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions")
        return Promise.resolve({
          includedCount: 1,
          excludedCount: 0,
          includedBytes: 1,
          includedSample: ["a"],
          excludedSample: [],
          truncated: false,
        });
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    const editButton = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.editExclusionsButton"));
    await editButton!.trigger("click");
    await flushPromises();
    const toggle = wrapper.get('[data-testid="placeholder-policy-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(true);
  });
});

describe("AddSourceWizard", () => {
  it("walks local -> drive -> exclusions -> encryption -> confirm and adds the source", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts")
        return Promise.resolve([
          {
            id: "acc-1",
            email: "user@example.com",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
          },
        ]);
      if (cmd === "pick_drive_folder")
        return Promise.resolve({
          currentFolderId: "root",
          currentFolderPath: "",
          folders: [{ id: "f-docs", name: "Docs" }],
        });
      if (cmd === "preview_exclusions")
        return Promise.resolve({
          includedCount: 10,
          excludedCount: 2,
          includedBytes: 2048,
          includedSample: ["x"],
          excludedSample: ["y"],
          truncated: false,
        });
      if (cmd === "pick_folder_dialog")
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      if (cmd === "add_source")
        // B3: unencrypted add returns no recovery phrase.
        return Promise.resolve({
          source: makeSource({ id: "src-new" }),
          recoveryPhrase: null,
        });
      if (cmd === "list_sources") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();

    // Step 1: choose local folder via the BACKEND dialog (C1: dialog-derived
    // path + one-shot token).
    const chooseLocal = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"));
    await chooseLocal!.trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("pick_folder_dialog", undefined);
    expect(wrapper.get('[data-testid="local-path"]').text()).toBe("/home/u/docs");

    const clickNext = async () => {
      const next = wrapper.findAll("button").find((b) => b.text() === i18n.global.t("common.next"));
      await next!.trigger("click");
      await flushPromises();
    };

    // -> Drive step (loads root listing).
    await clickNext();
    expect(invokeMock).toHaveBeenCalledWith("pick_drive_folder", {
      accountId: "acc-1",
      startFolderId: null,
      driveId: null,
    });
    // -> Exclusions step (loads preview).
    await clickNext();
    expect(wrapper.find('[data-testid="exclusion-preview"]').exists()).toBe(true);
    // -> Encryption step (encryption left off, no confirm gate).
    await clickNext();
    // -> Confirm step.
    await clickNext();
    expect(wrapper.find('[data-testid="confirm-summary"]').exists()).toBe(true);

    const finish = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.finish"));
    await finish!.trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("add_source", {
      req: expect.objectContaining({
        accountId: "acc-1",
        localPathToken: "tok-folder",
        localPath: "/home/u/docs",
        driveFolderId: "root",
        encryptionEnabled: false,
        respectGitignore: true,
      }),
    });
    expect(wrapper.emitted("created")).toBeTruthy();
  });

  it("issue #4: checking the cloud-only toggle sends placeholderPolicy force_download; default is skip", async () => {
    let addArgs: unknown = null;
    invokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "list_accounts")
        return Promise.resolve([
          {
            id: "acc-1",
            email: "user@example.com",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
          },
        ]);
      if (cmd === "pick_drive_folder")
        return Promise.resolve({
          currentFolderId: "root",
          currentFolderPath: "",
          folders: [{ id: "f-docs", name: "Docs" }],
        });
      if (cmd === "preview_exclusions")
        return Promise.resolve({
          includedCount: 1,
          excludedCount: 0,
          includedBytes: 1,
          includedSample: ["x"],
          excludedSample: [],
          truncated: false,
        });
      if (cmd === "pick_folder_dialog")
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      if (cmd === "add_source") {
        addArgs = args;
        return Promise.resolve({
          source: makeSource({ id: "src-new", placeholderPolicy: "force_download" }),
          recoveryPhrase: null,
          pendingRecoveryAck: false,
        });
      }
      if (cmd === "list_sources") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();

    const chooseLocal = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"));
    await chooseLocal!.trigger("click");
    await flushPromises();

    const clickNext = async () => {
      const next = wrapper.findAll("button").find((b) => b.text() === i18n.global.t("common.next"));
      await next!.trigger("click");
      await flushPromises();
    };

    await clickNext(); // -> drive
    await clickNext(); // -> exclusions

    // The toggle defaults unchecked; turn it on so the add carries force_download.
    const toggle = wrapper.get('[data-testid="placeholder-policy-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(false);
    await toggle.setValue(true);

    await clickNext(); // -> encryption
    await clickNext(); // -> confirm
    const finish = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.finish"));
    await finish!.trigger("click");
    await flushPromises();

    expect(addArgs).toMatchObject({
      req: { placeholderPolicy: "force_download" },
    });
  });

  it("does not accept a typed local path - only the dialog result", async () => {
    invokeMock.mockResolvedValue([]);
    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();
    // There is no text input for the local path anywhere in the wizard; the
    // only way to set it is the dialog (mocked above). Assert the absence.
    const textInputs = wrapper.findAll('input[type="text"]');
    const pathInputs = textInputs.filter((i) =>
      (i.element as HTMLInputElement).value.includes("/")
    );
    expect(pathInputs).toHaveLength(0);
  });

  it("persists the client-maintained Drive breadcrumb (R4-P2-2)", async () => {
    // R4-P2-2: pick_drive_folder returns an empty currentFolderPath (the backend
    // lists one folder's children, not the ancestor chain). The wizard builds
    // the breadcrumb itself in `crumbs` (parent/name) and must persist THAT path
    // - not the empty backend value - so backup_sources.drive_folder_path is the
    // real folder path, not blank. Drive it through the UI: descend into a
    // folder and assert the rendered Drive-folder path reflects the breadcrumb,
    // then finish and assert add_source receives the non-empty driveFolderPath.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts")
        return Promise.resolve([{ id: "acc-1", state: "active", label: "a", createdAt: 0 }]);
      if (cmd === "pick_folder_dialog")
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      if (cmd === "pick_drive_folder")
        return Promise.resolve({
          currentFolderId: "fid",
          currentFolderPath: "", // backend always blank
          folders: [{ id: "f-docs", name: "Docs" }],
        });
      if (cmd === "preview_exclusions")
        return Promise.resolve({
          includedCount: 1,
          excludedCount: 0,
          includedBytes: 1,
          includedSample: ["a"],
          excludedSample: [],
          truncated: false,
        });
      if (cmd === "add_source")
        return Promise.resolve({
          source: makeSource({ driveFolderPath: "Docs" }),
          recoveryPhrase: null,
          pendingRecoveryAck: false,
        });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();

    // Step 1: choose the local folder via the backend dialog.
    const chooseLocal = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"));
    await chooseLocal!.trigger("click");
    await flushPromises();

    const clickNext = async () => {
      const next = wrapper.findAll("button").find((b) => b.text() === i18n.global.t("common.next"));
      await next!.trigger("click");
      await flushPromises();
    };

    // -> Drive step: root listing loaded, destination shows My Drive root.
    await clickNext();
    const driveLabel = i18n.global.t("drivePicker.destinationLabel");
    expect(wrapper.text()).toContain(`${driveLabel}:`);

    // Click the "Docs" folder to descend; the rendered path must now be "Docs",
    // proving the client breadcrumb was persisted (not the empty backend value).
    const docsBtn = wrapper.findAll("button").find((b) => b.text() === "Docs");
    await docsBtn!.trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain(`${driveLabel}: Docs`);

    // Finish: add_source receives the breadcrumb path, not a blank string.
    await clickNext(); // -> exclusions
    await clickNext(); // -> encryption
    await clickNext(); // -> confirm
    const finish = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.finish"));
    await finish!.trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("add_source", {
      req: expect.objectContaining({ driveFolderPath: "Docs" }),
    });
  });
});

describe("Settings Rules tab", () => {
  it("loads settings and round-trips a toggle + a numeric field", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("get_settings", undefined);
    const form = wrapper.get('[data-testid="rules-form"]');

    // Toggle skip-on-battery off. The first checkbox in the form is the
    // Startup auto-start toggle; skip-on-battery is the next one.
    const batteryCheckbox = form.findAll('input[type="checkbox"]')[1];
    await batteryCheckbox.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { skipOnBattery: false } },
    });

    // Set the bandwidth cap (empty = unlimited -> 50 Mbps).
    const numberInputs = form.findAll('input[type="number"]');
    const bandwidthInput = numberInputs[0];
    await bandwidthInput.setValue("50");
    await bandwidthInput.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { bandwidthCapMbps: 50 } },
    });
  });

  it("shows the degraded locked-file-backup banner when the helper status says so", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status")
        return Promise.resolve({
          supported: true,
          elevated: false,
          helperEnabled: false,
          helperAlive: false,
          helperLaunchable: false,
          lockedFileBackupDegraded: true,
        });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("get_vss_helper_status", undefined);
    expect(wrapper.find('[data-testid="vss-degraded-banner"]').exists()).toBe(true);
    expect(wrapper.get('[data-testid="vss-degraded-banner"]').text()).toContain(
      "Locked files are being skipped"
    );
  });

  it("hides the degraded banner when locked-file backup is available", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status")
        return Promise.resolve({
          supported: true,
          elevated: true,
          helperEnabled: false,
          helperAlive: false,
          helperLaunchable: false,
          lockedFileBackupDegraded: false,
        });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    expect(wrapper.find('[data-testid="vss-degraded-banner"]').exists()).toBe(false);
  });

  it("startup: auto-start renders ON by default and toggling it patches the preference", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ global: { ...makeSettings().global, autoStartOnLogin: true } })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: { global?: Record<string, unknown> } }).patch;
        const base = makeSettings();
        return Promise.resolve({
          ...base,
          global: { ...base.global, ...(patch.global ?? {}) },
        });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    // Default ON: the toggle reflects the persisted preference.
    const toggle = wrapper.get('[data-testid="autostart-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(true);

    // Turning it off patches the persisted preference (the backend then
    // unregisters the OS startup entry).
    await toggle.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { autoStartOnLogin: false } },
    });
  });

  it("advanced: small-file bundling renders OFF by default and toggling it patches the top-level flag", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Partial<SettingsDto> }).patch;
        return Promise.resolve({ ...makeSettings(), ...patch });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    // Default OFF: the frozen v1.0.0 behaviour.
    const toggle = wrapper.get('[data-testid="bundle-small-files-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(false);

    // Turning it on patches the standalone top-level flag (NOT a global-group
    // field), which the backend writes to the `bundle_small_files` KV key.
    await toggle.setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { bundleSmallFiles: true },
    });
  });

  it("schedule window: enable, edit time, and toggle a day each patch the schedule", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    type SchedPatch = { patch?: { global?: { schedule?: ScheduleSettings } } };
    const lastSchedule = (): ScheduleSettings | undefined =>
      invokeMock.mock.calls
        .filter((c) => c[0] === "update_settings" && (c[1] as SchedPatch).patch?.global?.schedule)
        .map((c) => (c[1] as SchedPatch).patch!.global!.schedule!)
        .pop();

    // The time/day controls are hidden until the schedule is enabled.
    expect(wrapper.find('input[type="time"]').exists()).toBe(false);

    await wrapper.get('[data-testid="schedule-enabled"]').setValue(true);
    await flushPromises();
    const enabled = lastSchedule();
    expect(enabled?.enabled).toBe(true);
    expect(enabled?.days).toHaveLength(7);
    expect(typeof enabled?.utcOffsetMinutes).toBe("number");

    // The window controls are now visible; editing the start time re-patches
    // the local minute-of-day (09:30 -> 570).
    const start = wrapper.get('input[type="time"]');
    await start.setValue("09:30");
    await start.trigger("change");
    await flushPromises();
    expect(lastSchedule()?.startMinute).toBe(9 * 60 + 30);

    // Toggling the Sunday (index 0) button flips that day off.
    const dayButtons = wrapper.findAll('[data-testid="schedule-setting"] button');
    expect(dayButtons).toHaveLength(7);
    await dayButtons[0].trigger("click");
    await flushPromises();
    expect(lastSchedule()?.days[0]).toBe(false);
  });

  it("metered: switching to throttle patches the mode and reveals the cap input", async () => {
    // Deep-merge the global on round-trip so the metered section (gated on
    // skipOnMetered) stays rendered after the mode patch.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: { global?: Record<string, unknown> } }).patch;
        const base = makeSettings();
        return Promise.resolve({
          ...base,
          global: { ...base.global, ...(patch.global ?? {}) },
        });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    // In pause mode the throttle cap input is hidden.
    expect(wrapper.find('[data-testid="metered-setting"] input[type="number"]').exists()).toBe(
      false
    );

    await wrapper.get('[data-testid="metered-mode"]').setValue("throttle");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { meteredMode: "throttle" } },
    });

    // The cap input now appears; setting it patches the metered cap.
    const cap = wrapper.get('[data-testid="metered-setting"] input[type="number"]');
    await cap.setValue("5");
    await cap.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { meteredBandwidthCapMbps: 5 } },
    });
  });

  it("backup hooks: setting a command patches it, clearing patches null", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    type GlobalPatch = { patch?: { global?: Record<string, unknown> } };
    const lastGlobalPatch = (key: string): unknown =>
      invokeMock.mock.calls
        .filter(
          (c) => c[0] === "update_settings" && key in ((c[1] as GlobalPatch).patch?.global ?? {})
        )
        .map((c) => (c[1] as GlobalPatch).patch!.global![key])
        .pop();

    // Set a pre-backup hook command.
    const pre = wrapper.get('[data-testid="pre-hook"]');
    await pre.setValue("./backup-pre.sh");
    await pre.trigger("change");
    await flushPromises();
    expect(lastGlobalPatch("preBackupHook")).toBe("./backup-pre.sh");

    // Clearing the post-hook patches null (no hook).
    const post = wrapper.get('[data-testid="post-hook"]');
    await post.setValue("   ");
    await post.trigger("change");
    await flushPromises();
    expect(lastGlobalPatch("postBackupHook")).toBeNull();
  });

  it("custom root CA: a valid path validates, shows the cert count, and patches (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_custom_ca") {
        expect((args as { path: string }).path).toBe("/etc/corp/ca.pem");
        return Promise.resolve({ certCount: 2 });
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    const input = wrapper.get('[data-testid="custom-ca-path"]');
    await input.setValue("/etc/corp/ca.pem");
    await input.trigger("change");
    await flushPromises();

    // Validated (cert count surfaced) AND persisted.
    expect(invokeMock).toHaveBeenCalledWith("validate_custom_ca", { path: "/etc/corp/ca.pem" });
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { customRootCaPath: "/etc/corp/ca.pem" } },
    });
    const feedback = wrapper.get('[data-testid="custom-ca-feedback"]');
    expect(feedback.text()).toContain("2");
  });

  it("custom root CA: an invalid file surfaces an error and is NOT persisted (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_custom_ca") {
        return Promise.reject({ code: "internal.invalid_input", message: "bad pem" });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    const input = wrapper.get('[data-testid="custom-ca-path"]');
    await input.setValue("/etc/corp/broken.pem");
    await input.trigger("change");
    await flushPromises();

    // Error feedback shown; the bad path is NOT saved (no update_settings call).
    expect(wrapper.find('[data-testid="custom-ca-feedback"]').exists()).toBe(true);
    const savedCa = invokeMock.mock.calls.some((c) => c[0] === "update_settings");
    expect(savedCa).toBe(false);
  });

  it("custom root CA: clearing the path patches null (back to system trust) (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            global: { ...makeSettings().global, customRootCaPath: "/etc/corp/ca.pem" },
          })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    const input = wrapper.get('[data-testid="custom-ca-path"]');
    await input.setValue("   ");
    await input.trigger("change");
    await flushPromises();

    // A blank path clears the setting (null) WITHOUT calling validate.
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { customRootCaPath: null } },
    });
    expect(invokeMock.mock.calls.some((c) => c[0] === "validate_custom_ca")).toBe(false);
  });

  it("an empty bandwidth cap patches null (unlimited)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            global: { ...makeSettings().global, bandwidthCapMbps: 25 },
          })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();
    const form = wrapper.get('[data-testid="rules-form"]');
    const bandwidthInput = form.findAll('input[type="number"]')[0];
    await bandwidthInput.setValue("");
    await bandwidthInput.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { bandwidthCapMbps: null } },
    });
  });

  it("clamps out-of-range numeric inputs to the backend range before patching", async () => {
    // Regression: a plausible out-of-range value (100 concurrent uploads, a 10s
    // scan interval) must be clamped client-side so it never round-trips to a
    // backend rejection - that rejection used to brick the entire Rules form.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, { props: { tab: "rules" }, global: globalMountOptions });
    await flushPromises();
    const nums = wrapper.get('[data-testid="rules-form"]').findAll('input[type="number"]');
    // Order in the form: [bandwidth, concurrent, scan, deepVerify, hook].
    await nums[1].setValue("100"); // concurrent uploads, backend max 32
    await nums[1].trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { defaultConcurrentUploads: 32 } },
    });
    await nums[2].setValue("10"); // scan interval, backend min 30
    await nums[2].trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { scanIntervalSecs: 30 } },
    });
  });

  it("keeps the Rules form visible with a localized banner when a patch is rejected", async () => {
    // Regression: a rejected patch must NOT replace the whole form with the raw
    // error ("[object Object]") and brick the page. The form stays mounted and an
    // inline, localized error banner appears so the user can correct the value.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings")
        return Promise.reject({ code: "internal.invalid_input", message: "out of range" });
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, { props: { tab: "rules" }, global: globalMountOptions });
    await flushPromises();
    // Any commit that patches: toggle "pause on battery".
    const battery = wrapper.get('[data-testid="rules-form"]').findAll('input[type="checkbox"]')[0];
    await battery.setValue(false);
    await battery.trigger("change");
    await flushPromises();
    // The form is STILL mounted (not bricked) ...
    expect(wrapper.find('[data-testid="rules-form"]').exists()).toBe(true);
    // ... and a localized banner shows the error - never "[object Object]".
    const banner = wrapper.get('[data-testid="rules-error"]');
    expect(banner.text().length).toBeGreaterThan(0);
    expect(banner.text()).not.toContain("[object Object]");
  });

  it("changes the Windows VSS mode when the windows settings group is present", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();
    const form = wrapper.get('[data-testid="rules-form"]');
    const selects = form.findAll("select");
    // [0] = io priority, [1] = vss mode (windows present in the fake).
    const vssSelect = selects[selects.length - 1];
    await vssSelect.setValue("never");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { windows: { vssMode: "never" } },
    });
  });

  it("issue #25: renders the VSS helper toggle and toggling it patches windows.vssHelper", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status")
        return Promise.resolve({
          supported: true,
          elevated: false,
          helperEnabled: false,
          helperAlive: false,
          helperLaunchable: true,
          lockedFileBackupDegraded: false,
        });
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="vss-helper-toggle"]');
    // Reflects the stored setting (default off).
    expect((toggle.element as HTMLInputElement).checked).toBe(false);
    await toggle.setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { windows: { vssHelper: true } },
    });
  });

  it("issue #25: toggling the VSS helper survives a failing status re-fetch", async () => {
    // The setVssHelper handler re-fetches get_vss_helper_status after committing;
    // a rejection there must be swallowed (no unhandled rejection, no crash) - the
    // commit still lands.
    let statusCalls = 0;
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status") {
        statusCalls += 1;
        // First call (on tab activation) resolves; the post-toggle re-fetch rejects.
        return statusCalls === 1
          ? Promise.resolve({
              supported: true,
              elevated: false,
              helperEnabled: false,
              helperAlive: false,
              helperLaunchable: true,
              lockedFileBackupDegraded: false,
            })
          : Promise.reject(new Error("status unavailable"));
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="vss-helper-toggle"]');
    await toggle.setValue(true);
    await flushPromises();

    // The commit still happened despite the failing status re-fetch.
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { windows: { vssHelper: true } },
    });
    // The degraded banner is not shown (status went null on the rejection).
    expect(wrapper.find('[data-testid="vss-degraded-banner"]').exists()).toBe(false);
  });

  it("issue #25: enabling the helper shows the waiting-for-approval hint, then resolves on poll", async () => {
    vi.useFakeTimers();
    try {
      let statusCall = 0;
      invokeMock.mockImplementation((cmd: string, args: unknown) => {
        if (cmd === "get_settings") return Promise.resolve(makeSettings());
        if (cmd === "get_vss_helper_status") {
          statusCall += 1;
          // On tab load: not degraded. After enabling: pending. On the first poll:
          // declined (the user dismissed the UAC prompt).
          if (statusCall === 1) {
            return Promise.resolve({
              supported: true,
              elevated: false,
              helperEnabled: false,
              helperAlive: false,
              helperLaunchable: true,
              launchPending: false,
              launchDeclined: false,
              lockedFileBackupDegraded: false,
            });
          }
          if (statusCall === 2) {
            return Promise.resolve({
              supported: true,
              elevated: false,
              helperEnabled: true,
              helperAlive: false,
              helperLaunchable: true,
              launchPending: true,
              launchDeclined: false,
              lockedFileBackupDegraded: false,
            });
          }
          return Promise.resolve({
            supported: true,
            elevated: false,
            helperEnabled: true,
            helperAlive: false,
            helperLaunchable: false,
            launchPending: false,
            launchDeclined: true,
            lockedFileBackupDegraded: true,
          });
        }
        if (cmd === "update_settings") {
          const patch = (args as { patch: Record<string, unknown> }).patch;
          return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
        }
        return Promise.resolve(undefined);
      });
      const wrapper = mount(Settings, {
        props: { tab: "rules" },
        global: globalMountOptions,
      });
      await flushPromises();

      const toggle = wrapper.get('[data-testid="vss-helper-toggle"]');
      await toggle.setValue(true);
      await flushPromises();

      // The eager enable committed and the pending hint is shown.
      expect(invokeMock).toHaveBeenCalledWith("update_settings", {
        patch: { windows: { vssHelper: true } },
      });
      expect(wrapper.find('[data-testid="vss-helper-pending"]').exists()).toBe(true);

      // Advance the poll: the launch resolves to declined -> declined hint shown.
      await vi.advanceTimersByTimeAsync(1600);
      await flushPromises();
      expect(wrapper.find('[data-testid="vss-helper-pending"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="vss-helper-declined"]').exists()).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it("reflects telemetry default ON and toggling it calls set_telemetry_enabled (SPEC s16 R2-P1-1)", async () => {
    // M9b R2-P1-1: the "Send anonymous usage stats" toggle reflects the stored
    // telemetry.enabled (default ON) and unchecking it calls the DEDICATED
    // set_telemetry_enabled command (NOT a generic update_settings patch), so the
    // backend flips the in-flight ping cancel flag immediately (opt-out honored
    // mid-ping).
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "set_telemetry_enabled") return Promise.resolve(false);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(Settings, {
      props: { tab: "rules" },
      global: globalMountOptions,
    });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="telemetry-toggle"]');
    // Default ON: the box is checked.
    expect((toggle.element as HTMLInputElement).checked).toBe(true);
    // The privacy note is shown.
    expect(wrapper.get('[data-testid="telemetry-setting"]').text()).toContain(
      i18n.global.t("settings.rules.telemetryNote")
    );

    // Uncheck -> calls set_telemetry_enabled(false), NOT update_settings.
    await toggle.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("set_telemetry_enabled", {
      enabled: false,
    });
    expect(invokeMock).not.toHaveBeenCalledWith("update_settings", {
      patch: { telemetry: { enabled: false } },
    });
  });
});
