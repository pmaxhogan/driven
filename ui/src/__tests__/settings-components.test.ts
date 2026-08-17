// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import type { SettingsDto, SourceDto } from "../ipc/types";

// Component tests for the M6 settings UI: the SourceTable row actions, the
// AddSourceWizard multi-step flow, and the Rules-tab round-trip. They drive the
// real components against a faked backend (the `invoke` seam) + a faked
// tauri-plugin-dialog (so the folder pickers resolve deterministically), and
// assert that the right IPC commands fire with the right argument shapes.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
// The exclusion preview's live tree is event-driven, so the `listen` seam has to
// hand back the handlers a test can fire batches through. Every other event just
// resolves to an inert unlisten as before.
let previewBatchHandler: ((payload: unknown) => void) | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
    if (event === "exclusion_preview:batch") {
      previewBatchHandler = (payload: unknown) => cb({ payload });
    }
    return () => undefined;
  }),
}));
const openDialogMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openDialogMock(...args),
}));
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("0.1.0"),
}));

// vue-router: most of this file only needs useRouter().push (AccountList) - a
// fake push-only implementation is enough there and is kept for those tests.
// The "Settings About tab" describe below is the exception: it mounts the
// Settings SHELL, which now renders real routed children through
// <RouterView> and a <SettingsNav> full of <RouterLink>s (SDD 2026-08-02
// settings-sidebar-ia, task 3) - so it needs the REAL router primitives
// alongside the fake `useRouter`/`useRoute` (which RouterView/RouterLink do
// not consume - they resolve the router via internal inject keys, not the
// public composables re-exported here).
const pushMock = vi.fn();
vi.mock("vue-router", async () => {
  const actual = await vi.importActual<typeof import("vue-router")>("vue-router");
  return {
    ...actual,
    useRouter: () => ({ push: pushMock }),
    useRoute: () => ({ params: {} }),
  };
});

import { createMemoryHistory } from "vue-router";
import SourceTable from "../components/SourceTable.vue";
import AddSourceWizard from "../components/AddSourceWizard.vue";
import Settings from "../views/Settings.vue";
import { createAppRouter } from "../router";

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
      adaptiveParallelismEnabled: true,
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
      proxyMode: "system",
      proxyUrl: null,
      pauseWhenOffline: true,
    },
    telemetry: {
      enabled: true,
      installId: "id",
      endpoint: "https://example.test/ping",
    },
    updater: { channel: "stable", checkIntervalSecs: 21600 },
    ui: { trayLeftClickOpens: "activity", locale: "en-US", colorMode: "system" },
    windows: { vssMode: "auto", vssHelper: false },
    // null off macOS - the default fixture stands in for a non-mac host, so the
    // APFS block is absent unless a test opts in with `makeSettings({ macos })`.
    macos: null,
    bundleSmallFiles: false,
    scrub: { enabled: true, intervalSecs: 604800, sliceSize: 500, deepSample: 0 },
    drill: { enabled: true, intervalSecs: 2592000, sampleSize: 3 },
    ...over,
  };
}

const globalMountOptions = { plugins: [i18n] };

/** Pretend the webview is running on `ua`'s platform for the mounts that follow.
 *
 * The OneDrive / cloud-only placeholder policy is now hidden off Windows (it does
 * nothing there), and the components read the platform at SETUP time - so the
 * Windows-behaviour tests below have to say they are on Windows. jsdom's default
 * user-agent is neither Windows nor macOS. */
function setPlatformUserAgent(ua: string): void {
  Object.defineProperty(window.navigator, "userAgent", { value: ua, configurable: true });
}
const WINDOWS_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";
const MACOS_UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15";

beforeEach(() => {
  setActivePinia(createPinia());
  previewBatchHandler = null;
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

  it("states the version-retention behaviour of THIS source's destination", async () => {
    // The versioning panel used to assert Drive's behaviour ("kept in Drive's
    // trash, purged after ~30 days") for every source, which flatly contradicted
    // the S3 setup screen's own "S3 has no trash" warning in the same app. It is
    // not a wording problem: Drive really does keep a superseded object in its
    // trash, and S3 really does overwrite the previous copy. So the note is
    // resolved from the owning ACCOUNT's backend.
    const openPanelFor = async (backendKind: string) => {
      invokeMock.mockImplementation((cmd: string) => {
        if (cmd === "list_sources") return Promise.resolve([makeSource({ accountId: "acc-1" })]);
        if (cmd === "list_accounts")
          return Promise.resolve([
            {
              id: "acc-1",
              email: "dest",
              displayName: null,
              state: "ok",
              encryptionEnabled: false,
              createdAt: 0,
              lastSyncedAt: null,
              backendKind,
            },
          ]);
        if (cmd === "get_source_versioning")
          return Promise.resolve({ enabled: false, countCap: 10, maxBytes: 0 });
        return Promise.resolve(undefined);
      });
      const wrapper = mount(SourceTable, { global: globalMountOptions });
      await flushPromises();
      await wrapper.get('[data-testid="versioning-button"]').trigger("click");
      await flushPromises();
      return wrapper.get('[data-testid="versioning-retention"]').text();
    };

    // Drive: the trash caveat is CORRECT and must be kept.
    expect(await openPanelFor("google_drive")).toBe(i18n.global.t("versionRetention.google_drive"));

    // S3: no trash, and the previous copy is overwritten - so it must NOT be told
    // about a trash it does not have.
    const s3Note = await openPanelFor("s3");
    expect(s3Note).toBe(i18n.global.t("versionRetention.s3"));
    expect(s3Note).not.toContain("trash");

    // An unknown backend (BackendKind::ALL is Rust-owned and can gain entries
    // ahead of the locale file) falls back to the neutral line, never to Drive's.
    const unknownNote = await openPanelFor("some_future_backend");
    expect(unknownNote).toBe(i18n.global.t("versionRetention.default"));
    expect(unknownNote).not.toContain("trash");
  });

  // --- Issue #220: the versioning capability gate ---------------------------

  /** `list_backends()` as the Rust `descriptors()` reports it: only Google Drive
   * can really keep previous versions. S3, the local folder and SFTP all derive
   * an object's key from the file name, so a re-upload overwrites the previous
   * copy. */
  const VERSIONING_BACKENDS = [
    {
      id: "google_drive",
      usesOauth: true,
      supportsFolderPicker: true,
      supportsVersionHistory: true,
      isDefault: true,
    },
    {
      id: "s3",
      usesOauth: false,
      supportsFolderPicker: true,
      supportsVersionHistory: false,
      isDefault: false,
    },
    {
      id: "local_folder",
      usesOauth: false,
      supportsFolderPicker: false,
      supportsVersionHistory: false,
      isDefault: false,
    },
    {
      id: "sftp",
      usesOauth: false,
      supportsFolderPicker: true,
      supportsVersionHistory: false,
      isDefault: false,
    },
  ];

  /** Mount SourceTable with one source on a `backendKind` account, then open its
   * versioning panel. `stored` is what `get_source_versioning` returns. */
  async function openVersioningOn(
    backendKind: string,
    stored: { enabled: boolean; countCap: number; maxBytes: number } = {
      enabled: false,
      countCap: 10,
      maxBytes: 0,
    },
    onSet?: (args: unknown) => void
  ) {
    invokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource({ accountId: "acc-1" })]);
      if (cmd === "list_accounts")
        return Promise.resolve([
          {
            id: "acc-1",
            email: "dest",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
            backendKind,
          },
        ]);
      if (cmd === "list_backends") return Promise.resolve(VERSIONING_BACKENDS);
      if (cmd === "get_source_versioning") return Promise.resolve(stored);
      if (cmd === "set_source_versioning") {
        onSet?.(args);
        return Promise.resolve({ ...stored, enabled: false });
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    await wrapper.get('[data-testid="versioning-button"]').trigger("click");
    await flushPromises();
    return wrapper;
  }

  it("does not offer the versioning editor on a destination that cannot keep versions (issue #220)", async () => {
    // The defect this gate closes: per-source versioning promises "restore this
    // source's files as they were on an earlier date", but on S3, local-folder
    // and SFTP destinations the create key is derived from the file name, so
    // the re-upload OVERWRITES the previous bytes. The retained version then
    // points at the current content and a point-in-time restore silently
    // returns today's file. Nothing tested versioning against a non-Drive
    // backend, which is why it shipped. The editor must not be offered where
    // it cannot work.
    for (const kind of ["s3", "local_folder", "sftp"]) {
      const wrapper = await openVersioningOn(kind);
      // No control that could switch on a promise the destination cannot keep.
      expect(wrapper.find('[data-testid="versioning-enabled"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="versioning-cap"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="versioning-save"]').exists()).toBe(false);
      // ...and the panel says so plainly, instead of the promise-shaped intro.
      expect(wrapper.find('[data-testid="versioning-unsupported"]').exists()).toBe(true);
      expect(wrapper.text()).not.toContain(i18n.global.t("settings.sources.versioning.intro"));
    }

    // Google Drive really does keep a superseded object (its create mints a new
    // file id and the old one lives on in Drive's trash), so the editor stays.
    const drive = await openVersioningOn("google_drive");
    expect(drive.find('[data-testid="versioning-enabled"]').exists()).toBe(true);
    expect(drive.find('[data-testid="versioning-save"]').exists()).toBe(true);
    expect(drive.find('[data-testid="versioning-unsupported"]').exists()).toBe(false);
  });

  it("the unsupported-destination copy resolves to real strings, not the key names", () => {
    // `en-US.json` has no key-parity lint, and a missing key makes `t()` return
    // the key itself - so a `toContain(t("..."))` assertion would pass even with
    // the copy absent. Pin that the keys actually exist.
    for (const key of [
      "settings.sources.versioning.unsupported",
      "settings.sources.versioning.staleEnabled",
      "settings.sources.versioning.disableButton",
      "restore.asOf.unsupported",
      // `intro` too: the gate test above asserts the promise-shaped copy is
      // ABSENT on an unsupported destination, and that assertion would pass
      // trivially if the key itself vanished.
      "settings.sources.versioning.intro",
    ]) {
      expect(i18n.global.te(key)).toBe(true);
    }
    // And that the offer-removal copy actually says versioning is unavailable
    // rather than restating the promise.
    expect(i18n.global.t("settings.sources.versioning.unsupported")).toContain("not available");
  });

  it("lets a source clear a STALE versioning flag on such a destination (issue #220)", async () => {
    // A source enabled before this gate existed (or by an older build, or by any
    // other caller) keeps a flag the destination cannot honour. Enabling is
    // refused, but DISABLING must stay open - otherwise the source is stuck
    // advertising a point-in-time capability forever. This is the whole remedy
    // path, so assert it really sends `enabled: false`: the loaded ref is `true`.
    let saved: unknown = null;
    const wrapper = await openVersioningOn(
      "s3",
      { enabled: true, countCap: 7, maxBytes: 42 },
      (args) => {
        saved = args;
      }
    );
    // The stale flag is called out rather than left to look like it is working.
    expect(wrapper.find('[data-testid="versioning-stale"]').exists()).toBe(true);
    await wrapper.get('[data-testid="versioning-disable"]').trigger("click");
    await flushPromises();
    expect(saved).toMatchObject({
      sourceId: "src-1",
      // The size guard and cap are PRESERVED; only `enabled` flips.
      config: { enabled: false, countCap: 7, maxBytes: 42 },
    });

    // A source that never had it on gets no stale warning and no remedy button -
    // there would be nothing to remedy.
    const clean = await openVersioningOn("s3");
    expect(clean.find('[data-testid="versioning-stale"]').exists()).toBe(false);
    expect(clean.find('[data-testid="versioning-disable"]').exists()).toBe(false);
  });

  it("surfaces a failure to clear the stale flag instead of implying it worked", async () => {
    // The remedy is the only way out of a dishonest stored setting, so a failed
    // attempt must not look like a success: silently closing (or leaving the panel
    // unchanged) would have the user believe they cleared a flag that still claims
    // a point-in-time restore.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource({ accountId: "acc-1" })]);
      if (cmd === "list_accounts")
        return Promise.resolve([
          {
            id: "acc-1",
            email: "dest",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
            backendKind: "s3",
          },
        ]);
      if (cmd === "list_backends") return Promise.resolve(VERSIONING_BACKENDS);
      if (cmd === "get_source_versioning")
        return Promise.resolve({ enabled: true, countCap: 7, maxBytes: 0 });
      if (cmd === "set_source_versioning") return Promise.reject(new Error("transient db error"));
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    await wrapper.get('[data-testid="versioning-button"]').trigger("click");
    await flushPromises();

    await wrapper.get('[data-testid="versioning-disable"]').trigger("click");
    await flushPromises();
    // The panel stays open and reports the failure.
    expect(wrapper.find('[data-testid="versioning-editor"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="versioning-error"]').exists()).toBe(true);
  });

  it("keeps offering versioning when the backend descriptors cannot be fetched", async () => {
    // The #219 house rule: an unknown / unloaded descriptor resolves PERMISSIVE so
    // a transient fetch failure never hides a control a Drive source needs. Safe
    // because `set_source_versioning` enforces the same gate server-side - the
    // worst a permissive default can do is render a Save that fails loudly.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource({ accountId: "acc-1" })]);
      if (cmd === "list_accounts")
        return Promise.resolve([
          {
            id: "acc-1",
            email: "dest",
            displayName: null,
            state: "ok",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
            backendKind: "s3",
          },
        ]);
      if (cmd === "list_backends") return Promise.reject(new Error("transient ipc error"));
      if (cmd === "get_source_versioning")
        return Promise.resolve({ enabled: false, countCap: 10, maxBytes: 0 });
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    await wrapper.get('[data-testid="versioning-button"]').trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="versioning-enabled"]').exists()).toBe(true);
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
    expect(invokeMock).toHaveBeenCalledWith("sync_now", {
      sourceId: "src-1",
      bypassGates: null,
    });
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

  it("keeps the source listed and shows the error when remote deletion fails (#227)", async () => {
    // Issue #227: `remove_source(delete_remote: true)` aborts with nothing
    // removed on a destination failure - backend-neutral now (S3, SFTP, the
    // local folder), not just Drive. The UI must surface that loudly rather
    // than silently doing nothing: the source stays listed, the confirm
    // panel stays open with the checkbox still ticked, and the error shows.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "remove_source") {
        return Promise.reject({ code: "drive.unreachable", message: "backend English" });
      }
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
    await confirmPanel.get('[data-testid="source-remove-confirm-button"]').trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("remove_source", {
      sourceId: "src-1",
      deleteRemote: true,
    });
    // Nothing was removed and the panel is still open for a retry.
    expect(wrapper.find('[data-testid="source-remove-confirm"]').exists()).toBe(true);
    expect((confirmPanel.get('input[type="checkbox"]').element as HTMLInputElement).checked).toBe(
      true
    );
    const err = wrapper.find('[data-testid="remove-error"]');
    expect(err.exists()).toBe(true);
    // The stable code is localized, never the raw backend message.
    expect(err.text()).not.toContain("backend English");
  });

  it("re-runs the streaming preview when a rule changes in the open editor", async () => {
    // The editor's panel lives inside the per-source v-for, so the tree's
    // template ref is registered in a v-for scope; a blur must still reach the
    // component's restart(). This is the whole point of the live preview - a
    // rule edit has to re-classify without closing and reopening the editor.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.editExclusionsButton"))!
      .trigger("click");
    await flushPromises();

    const editor = wrapper.get('[data-testid="exclusion-editor"]');
    const startsAfterOpen = invokeMock.mock.calls.filter(
      (c) => c[0] === "preview_exclusions_start"
    ).length;
    const excludeArea = editor.findAll("textarea")[1];
    await excludeArea.setValue("*.log");
    await excludeArea.trigger("blur");
    await flushPromises();

    const startsAfterBlur = invokeMock.mock.calls.filter(
      (c) => c[0] === "preview_exclusions_start"
    ).length;
    expect(startsAfterBlur).toBe(startsAfterOpen + 1);
    expect(invokeMock).toHaveBeenLastCalledWith(
      "preview_exclusions_start",
      expect.objectContaining({
        req: expect.objectContaining({ sourceId: "src-1", excludePatterns: ["*.log"] }),
      })
    );
  });

  it("warns while typing an include pattern that defeats directory pruning", async () => {
    // The scanner can only skip descending into an excluded folder when no
    // include rule could match beneath it. A relative rule (or one with a
    // double-star) forces it into every node_modules, so the editor calls that
    // out AS THE USER TYPES - the preview walk only re-runs on blur, and the
    // guidance must not wait for it.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.editExclusionsButton"))!
      .trigger("click");
    await flushPromises();

    const editor = wrapper.get('[data-testid="exclusion-editor"]');
    // A source with no include rules at all has nothing to warn about.
    expect(wrapper.find('[data-testid="include-pattern-warning"]').exists()).toBe(false);

    const includeArea = editor.findAll("textarea")[0];
    await includeArea.setValue("/keep/.env\n.env\n/*/x/**/.env");
    const warning = wrapper.get('[data-testid="include-pattern-warning"]');
    expect(warning.text()).toContain(i18n.global.t("settings.addSource.includeWarning.title"));
    // Only the offending rules are listed - the anchored, depth-bounded one is
    // fine and must not be named.
    const listed = warning.findAll("li").map((li) => li.text());
    expect(listed).toEqual([".env", "/*/x/**/.env"]);

    // Anchoring them clears the box without a blur or a re-preview.
    await includeArea.setValue("/keep/.env\n/*/.env");
    expect(wrapper.find('[data-testid="include-pattern-warning"]').exists()).toBe(false);
  });

  it("Edit exclusions opens the inline editor and saves a patch", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
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
      "preview_exclusions_start",
      // R1-P1-2: an EXISTING source is previewed by its id (the backend resolves
      // the local path from SQLite), NEVER a raw webview path. The wrapper nests
      // the request under `req` (matching the Rust signature). The editor now
      // opens the STREAMING preview, which validates identically and then feeds
      // the live folder tree.
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
    // user turns it on, the saved patch carries "force_download". Windows-only:
    // the control is not offered on platforms where the policy does nothing.
    setPlatformUserAgent(WINDOWS_UA);
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([makeSource()]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
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

  it("hides the Windows-only cloud-only control off Windows, preserving the stored policy", async () => {
    // The control's own caption said it "applies to OneDrive / cloud-only
    // placeholder files on Windows (harmless elsewhere)" - so on macOS it was a
    // setting that admitted it did nothing. Hiding it must NOT silently rewrite
    // the source's policy: a force_download source edited on macOS keeps
    // force_download, because the value is seeded from the row and written back.
    setPlatformUserAgent(MACOS_UA);
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([makeSource({ placeholderPolicy: "force_download" })]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
      if (cmd === "update_source")
        return Promise.resolve(makeSource({ placeholderPolicy: "force_download" }));
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.sources.editExclusionsButton"))!
      .trigger("click");
    await flushPromises();

    const editor = wrapper.get('[data-testid="exclusion-editor"]');
    expect(editor.find('[data-testid="placeholder-policy-toggle"]').exists()).toBe(false);

    // Saving keeps the Windows-set policy rather than downgrading it to "skip".
    await editor
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.save"))!
      .trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_source", {
      sourceId: "src-1",
      patch: expect.objectContaining({ placeholderPolicy: "force_download" }),
    });
  });

  it("issue #4: the edit toggle reflects an already-force_download source", async () => {
    setPlatformUserAgent(WINDOWS_UA);
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([makeSource({ placeholderPolicy: "force_download" })]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
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
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
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

  it("a tree row's +/- appends the matching glob to the patterns and re-previews", async () => {
    // The end-to-end wiring of spec point (d): the wizard's exclusions step
    // renders the live tree, a row's action button emits the anchored glob, the
    // wizard appends it as a NEW LINE to the right textarea, and the walk
    // re-runs so the tree reflects the new rule. The globs asserted here are the
    // exact forms the Rust exclude tests verify against the real matcher.
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
        return Promise.resolve({ currentFolderId: "root", currentFolderPath: "", folders: [] });
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
      if (cmd === "pick_folder_dialog")
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"))!
      .trigger("click");
    await flushPromises();
    const clickNext = async () => {
      await wrapper
        .findAll("button")
        .find((b) => b.text() === i18n.global.t("common.next"))!
        .trigger("click");
      await flushPromises();
    };
    await clickNext(); // -> Drive
    await clickNext(); // -> Exclusions

    // R1-P1-2: a NEW candidate folder is previewed by its dialog TOKEN.
    expect(invokeMock).toHaveBeenCalledWith(
      "preview_exclusions_start",
      expect.objectContaining({
        req: expect.objectContaining({ localPathToken: "tok-folder" }),
      })
    );

    // Stream a tiny tree: an included file, an included folder, and an excluded
    // file (which the backend surfaces even though it will not be backed up).
    // A click re-runs the walk, which CLEARS the tree (every verdict is now
    // stale under the new rule) - so each step re-streams, exactly as the real
    // backend would.
    const streamTree = async () => {
      previewBatchHandler!({
        previewId: "gen-1",
        nodes: [
          { path: "keep.txt", isDir: false, included: true, size: 4 },
          { path: "build", isDir: true, included: true, size: 0 },
          { path: "secret.env", isDir: false, included: false, size: 2 },
        ],
        includedCount: 1,
        excludedCount: 1,
        includedBytes: 4,
        truncated: false,
      });
      await new Promise((r) => setTimeout(r, 25));
      await flushPromises();
    };
    await streamTree();

    const textareas = () => wrapper.findAll("textarea");
    // "-" on an INCLUDED file -> an anchored exclude glob.
    await wrapper.get('[data-testid="preview-action-keep.txt"]').trigger("click");
    await flushPromises();
    expect((textareas()[1].element as HTMLTextAreaElement).value).toBe("/keep.txt");
    await streamTree();

    // "-" on an INCLUDED folder -> the trailing-slash (whole subtree) form,
    // appended on its own line under the first rule.
    await wrapper.get('[data-testid="preview-action-build"]').trigger("click");
    await flushPromises();
    expect((textareas()[1].element as HTMLTextAreaElement).value).toBe(
      ["/keep.txt", "/build/"].join("\n")
    );
    await streamTree();

    // "+" on an EXCLUDED file -> the INCLUDE textarea, not the exclude one.
    await wrapper.get('[data-testid="preview-action-secret.env"]').trigger("click");
    await flushPromises();
    expect((textareas()[0].element as HTMLTextAreaElement).value).toBe("/secret.env");

    // Each click re-ran the walk with the rules as they then stood.
    expect(invokeMock).toHaveBeenLastCalledWith(
      "preview_exclusions_start",
      expect.objectContaining({
        req: expect.objectContaining({
          includePatterns: ["/secret.env"],
          excludePatterns: ["/keep.txt", "/build/"],
        }),
      })
    );
  });

  it("warns on the exclusions step when an include pattern defeats directory pruning", async () => {
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
        return Promise.resolve({ currentFolderId: "root", currentFolderPath: "", folders: [] });
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
      if (cmd === "pick_folder_dialog")
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"))!
      .trigger("click");
    await flushPromises();
    const clickNext = async () => {
      await wrapper
        .findAll("button")
        .find((b) => b.text() === i18n.global.t("common.next"))!
        .trigger("click");
      await flushPromises();
    };
    await clickNext(); // -> Drive
    await clickNext(); // -> Exclusions

    // A fresh wizard starts with no rules, so nothing to warn about.
    expect(wrapper.find('[data-testid="include-pattern-warning"]').exists()).toBe(false);

    const includeArea = wrapper.findAll("textarea")[0];
    await includeArea.setValue("blah/.env,/a/*/b/.env");
    const warning = wrapper.get('[data-testid="include-pattern-warning"]');
    expect(warning.text()).toContain(i18n.global.t("settings.addSource.includeWarning.hint"));
    expect(warning.findAll("li").map((li) => li.text())).toEqual(["blah/.env"]);

    // Anchoring the rule clears the box, and typing never triggered a re-walk
    // (that still belongs to blur).
    await includeArea.setValue("/blah/.env,/a/*/b/.env");
    expect(wrapper.find('[data-testid="include-pattern-warning"]').exists()).toBe(false);
  });

  it("issue #4: checking the cloud-only toggle sends placeholderPolicy force_download; default is skip", async () => {
    setPlatformUserAgent(WINDOWS_UA);
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
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
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

  it("hides the cloud-only toggle off Windows and still sends the default policy", async () => {
    // Hiding a control must not send an absent / undefined field: the add still
    // carries "skip", the same value a Windows user gets by leaving it unticked,
    // so a macOS-created source behaves identically on the same account.
    setPlatformUserAgent(MACOS_UA);
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
            backendKind: "google_drive",
          },
        ]);
      if (cmd === "pick_drive_folder")
        return Promise.resolve({ currentFolderId: "root", currentFolderPath: "", folders: [] });
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
      if (cmd === "pick_folder_dialog")
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      if (cmd === "add_source") {
        addArgs = args;
        return Promise.resolve({
          source: makeSource({ id: "src-new" }),
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
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"))!
      .trigger("click");
    await flushPromises();
    const clickNext = async () => {
      await wrapper
        .findAll("button")
        .find((b) => b.text() === i18n.global.t("common.next"))!
        .trigger("click");
      await flushPromises();
    };
    await clickNext(); // -> destination
    await clickNext(); // -> exclusions

    expect(wrapper.find('[data-testid="placeholder-policy-toggle"]').exists()).toBe(false);

    await clickNext(); // -> encryption
    await clickNext(); // -> confirm
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.finish"))!
      .trigger("click");
    await flushPromises();

    expect(addArgs).toMatchObject({ req: { placeholderPolicy: "skip" } });
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
      if (cmd === "preview_exclusions_start") return Promise.resolve("gen-1");
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

// About moved out of the top nav and, as of SDD 2026-08-02
// settings-sidebar-ia task 3, out of a tab strip too - it is reached ONLY
// through the SettingsNav sidebar footer (Locked decisions), which links
// /settings/about. /about (the old flat path) still resolves via a redirect.
// These cover the new shape: the footer link is offered and points at the
// right route, and that route renders the real About surface rather than a
// Rules page. (A synthetic click-then-assert-navigation test was tried and
// dropped: RouterLink's own click handling did not resolve in jsdom under
// VTU's `.trigger` nor a manually dispatched MouseEvent here, so the href
// target and the navigated-to content are asserted directly instead - which
// covers the same contract without depending on that click plumbing.)
describe("Settings About tab", () => {
  async function mountSettingsAt(path: string) {
    const router = createAppRouter(createMemoryHistory());
    await router.push(path);
    await router.isReady();
    const wrapper = mount(Settings, {
      global: { plugins: [...globalMountOptions.plugins, router] },
    });
    await flushPromises();
    return { wrapper, router };
  }

  it("offers a sidebar footer link to About", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const { wrapper } = await mountSettingsAt("/settings/accounts");

    const aboutLink = wrapper.find('[data-testid="settings-nav-about"]');
    expect(aboutLink.exists()).toBe(true);
    expect(aboutLink.attributes("href")).toBe("/settings/about");
  });

  it("renders the About surface (not a Rules page) on /settings/about", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      if (cmd === "list_releases") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    const { wrapper } = await mountSettingsAt("/settings/about");

    // The About surface's own controls are present...
    expect(wrapper.find('[data-testid="check-updates"]').exists()).toBe(true);
    expect(wrapper.text()).toContain(i18n.global.t("about.updatesTitle"));
    // ...the channel selector moved to GeneralPage (task 5) so it is gone
    // from here...
    expect(wrapper.find('[data-testid="channel-select"]').exists()).toBe(false);
    // ...and no Rules page is rendered in its place.
    expect(wrapper.find('[data-testid="rules-form"]').exists()).toBe(false);
  });
});
