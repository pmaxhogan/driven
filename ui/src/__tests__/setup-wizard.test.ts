// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";
import { createRouter, createMemoryHistory } from "vue-router";

// The setup wizard drives the SPEC s11.1 OAuth sequence + the first-source +
// initial-sync flow entirely through the typed IPC wrappers, which route to
// `@tauri-apps/api/core` `invoke`. Mocking that single seam (plus the event
// `listen` seam and the dialog `open` seam) lets us assert the wizard walks all
// five DESIGN s8.5 steps and fires the OAuth IPC calls in order against a fake
// backend - no Tauri runtime required.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

// Capture the registered `oauth:complete` handler so the test can fire the event
// the way the Rust loopback server would.
let oauthCompleteHandler: ((e: { payload: unknown }) => void) | null = null;
const unlistenMock = vi.fn();
vi.mock("@tauri-apps/api/event", () => ({
  listen: (event: string, handler: (e: { payload: unknown }) => void) => {
    if (event === "oauth:complete") oauthCompleteHandler = handler;
    return Promise.resolve(unlistenMock);
  },
}));

// The local folder picker is a tauri-plugin-dialog directory dialog; the mock
// returns a fixed dialog-derived path (SPEC s11.6.1: add_source must get a
// dialog-derived local path, never a webview string).
const dialogOpenMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (opts?: unknown) => dialogOpenMock(opts),
}));

import { i18n } from "../i18n";
import SetupWizard from "../views/SetupWizard.vue";
import { useSetupStore } from "../stores/setup";
import { useSourcesStore } from "../stores/sources";

// Fixed fakes for the backend responses.
const FAKE_SESSION = "sess-123";
const FAKE_ACCOUNT = {
  id: "acct-1",
  email: "user@example.com",
  displayName: null,
  state: "ok",
  encryptionEnabled: false,
  createdAt: 0,
  lastSyncedAt: null,
};
const FAKE_DRIVE_LISTING = {
  currentFolderId: "drive-folder-1",
  currentFolderPath: "/Backups/Docs",
  folders: [],
};
const FAKE_SOURCE = {
  id: "src-1",
  accountId: "acct-1",
  displayName: "Docs",
  enabled: true,
  localPath: "/home/user/Docs",
  driveFolderId: "drive-folder-1",
  driveFolderPath: "/Backups/Docs",
  encryptionEnabled: false,
  respectGitignore: true,
  includePatterns: [],
  excludePatterns: [],
  deepVerifyIntervalSecs: 0,
  lastFullScanAt: null,
  createdAt: 0,
};

/** Wire the fake backend: route each command name to its canned response. */
function installFakeBackend(): void {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "begin_add_account_wizard":
        return Promise.resolve(FAKE_SESSION);
      case "submit_oauth_credentials":
        return Promise.resolve(undefined);
      case "start_oauth_signin":
        return Promise.resolve({ authUrl: "https://accounts.google.com/o/x" });
      case "poll_oauth_status":
        return Promise.resolve({ kind: "complete" });
      case "finish_add_account":
        return Promise.resolve(FAKE_ACCOUNT);
      case "pick_drive_folder":
        return Promise.resolve(FAKE_DRIVE_LISTING);
      case "add_source":
        return Promise.resolve(FAKE_SOURCE);
      case "list_sources":
        return Promise.resolve([FAKE_SOURCE]);
      case "sync_now":
        return Promise.resolve(undefined);
      default:
        return Promise.resolve(undefined);
    }
  });
}

/** The OAuth command names in their SPEC s11.1 contractual order. */
function oauthCallOrder(): string[] {
  return invokeMock.mock.calls
    .map((c) => c[0] as string)
    .filter((cmd) =>
      [
        "begin_add_account_wizard",
        "submit_oauth_credentials",
        "start_oauth_signin",
        "poll_oauth_status",
        "finish_add_account",
      ].includes(cmd),
    );
}

beforeEach(() => {
  invokeMock.mockReset();
  unlistenMock.mockReset();
  dialogOpenMock.mockReset();
  oauthCompleteHandler = null;
  setActivePinia(createPinia());
  installFakeBackend();
  dialogOpenMock.mockResolvedValue("/home/user/Docs");
  // window.open is the browser opener seam; stub it so no real window opens.
  vi.stubGlobal("open", vi.fn());
});

describe("setup store OAuth sequence (SPEC s11.1)", () => {
  it("fires begin -> submit -> startSignin -> poll -> finish in order", async () => {
    const setup = useSetupStore();
    await setup.begin();
    await setup.connectAccount("client-id", "client-secret");
    const done = await setup.checkSigninComplete();

    expect(done).toBe(true);
    expect(setup.signedIn).toBe(true);
    expect(setup.accountId).toBe("acct-1");
    expect(oauthCallOrder()).toEqual([
      "begin_add_account_wizard",
      "submit_oauth_credentials",
      "start_oauth_signin",
      "poll_oauth_status",
      "finish_add_account",
    ]);
  });

  it("connectAccount opens the returned auth URL via the injected opener", async () => {
    const setup = useSetupStore();
    const opener = vi.fn();
    await setup.begin();
    await setup.connectAccount("cid", "csec", opener);
    expect(opener).toHaveBeenCalledWith("https://accounts.google.com/o/x");
    expect(setup.oauthStatus).toEqual({ kind: "awaitingCallback" });
  });

  it("createFirstSource adds the source with its encryption flag", async () => {
    const setup = useSetupStore();
    setup.accountId = "acct-1";
    setup.localPath = "/home/user/Docs";
    setup.driveFolderId = "drive-folder-1";
    setup.driveFolderPath = "/Backups/Docs";
    setup.encryptionEnabled = true;
    await setup.createFirstSource();

    const addCall = invokeMock.mock.calls.find((c) => c[0] === "add_source");
    expect(addCall).toBeDefined();
    expect(
      (addCall![1] as { req: { encryptionEnabled: boolean } }).req
        .encryptionEnabled,
    ).toBe(true);
    expect(setup.sourceId).toBe("src-1");
    // Reuses the sources store list (refresh fired after add).
    const sources = useSourcesStore();
    expect(sources.sources).toHaveLength(1);
  });

  it("startInitialSync scopes sync_now to the new source", async () => {
    const setup = useSetupStore();
    setup.sourceId = "src-1";
    await setup.startInitialSync();
    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: "src-1" });
  });

  it("checkSigninComplete records a failed code without advancing", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "begin_add_account_wizard") return Promise.resolve(FAKE_SESSION);
      if (cmd === "poll_oauth_status")
        return Promise.resolve({ kind: "failed", code: "auth.invalid_grant" });
      return Promise.resolve(undefined);
    });
    const setup = useSetupStore();
    await setup.begin();
    const done = await setup.checkSigninComplete();
    expect(done).toBe(false);
    expect(setup.errorCode).toBe("auth.invalid_grant");
  });
});

describe("SetupWizard walks all five steps (DESIGN s8.5)", () => {
  async function mountWizard() {
    // A throwaway memory router so useRouter()/router.push resolve in jsdom.
    // Only the two routes the wizard touches are needed.
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [
        { path: "/setup", name: "setup", component: { template: "<div />" } },
        {
          path: "/activity",
          name: "activity",
          component: { template: "<div />" },
        },
      ],
    });
    await router.push("/setup");
    await router.isReady();
    const wrapper = mount(SetupWizard, {
      global: { plugins: [i18n, router] },
    });
    await flushPromises();
    return { wrapper, router };
  }

  it("walks welcome -> credentials -> source -> encryption -> confirm and finishes", async () => {
    const { wrapper, router } = await mountWizard();
    const setup = useSetupStore();

    // Step 1: welcome. begin() fired on mount.
    expect(setup.step).toBe("welcome");
    expect(invokeMock).toHaveBeenCalledWith("begin_add_account_wizard", undefined);
    await wrapper.get("footer button:last-child").trigger("click");
    expect(setup.step).toBe("credentials");

    // Step 2: credentials. Paste + sign in, then fire oauth:complete.
    const inputs = wrapper.findAll("input");
    await inputs[0].setValue("client-id");
    await inputs[1].setValue("client-secret");
    // The "Sign in with Google" button is the first button in the step body.
    const stepButtons = wrapper.findAll("button");
    const signInBtn = stepButtons.find(
      (b) => b.text() === i18n.global.t("wizard.step2.signInButton"),
    );
    expect(signInBtn).toBeTruthy();
    await signInBtn!.trigger("click");
    await flushPromises();
    expect(setup.oauthStatus).toEqual({ kind: "awaitingCallback" });

    // The Rust loopback server reports completion.
    expect(oauthCompleteHandler).toBeTruthy();
    oauthCompleteHandler!({ payload: { session_id: "sess-123", status: {} } });
    await flushPromises();
    expect(setup.signedIn).toBe(true);
    expect(setup.step).toBe("source");

    // Step 3: pick local folder (dialog) + Drive destination (IPC).
    const chooseLocal = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("wizard.step3.chooseFolderButton"));
    await chooseLocal!.trigger("click");
    await flushPromises();
    expect(setup.localPath).toBe("/home/user/Docs");

    const chooseDrive = wrapper
      .findAll("button")
      .find(
        (b) => b.text() === i18n.global.t("settings.addSource.chooseDriveButton"),
      );
    await chooseDrive!.trigger("click");
    await flushPromises();
    expect(setup.driveFolderId).toBe("drive-folder-1");

    await wrapper.get("footer button:last-child").trigger("click");
    expect(setup.step).toBe("encryption");

    // Step 4: opt into encryption, then advance (creates the source).
    const encBox = wrapper.find('input[type="checkbox"]');
    await encBox.setValue(true);
    expect(setup.encryptionEnabled).toBe(true);
    await wrapper.get("footer button:last-child").trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith(
      "add_source",
      expect.objectContaining({
        req: expect.objectContaining({ encryptionEnabled: true }),
      }),
    );
    expect(setup.step).toBe("confirm");

    // Step 5: finish -> initial sync -> navigate to /activity.
    await wrapper.get("footer button:last-child").trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: "src-1" });
    expect(router.currentRoute.value.path).toBe("/activity");

    // The full OAuth sequence fired in contractual order.
    expect(oauthCallOrder()).toEqual([
      "begin_add_account_wizard",
      "submit_oauth_credentials",
      "start_oauth_signin",
      "poll_oauth_status",
      "finish_add_account",
    ]);
  });
});
