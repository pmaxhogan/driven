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
    },
    telemetry: {
      enabled: true,
      installId: "id",
      endpoint: "https://example.test/ping",
    },
    updater: { channel: "stable", checkIntervalSecs: 21600 },
    ui: { trayLeftClickOpens: "activity", locale: "en-US", colorMode: "system" },
    windows: { vssMode: "auto" },
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
      },
    });
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

    // -> Drive step: root listing loaded, path empty.
    await clickNext();
    const driveLabel = i18n.global.t("settings.addSource.step.driveFolder");
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

    // Toggle skip-on-battery off.
    const batteryCheckbox = form.get('input[type="checkbox"]');
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
