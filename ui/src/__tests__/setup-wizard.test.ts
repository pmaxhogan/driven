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

// The sign-in fix opens the system browser via the Tauri v2 opener plugin
// (NOT window.open, which does not reliably reach the system browser in a Tauri
// webview). Mock the plugin so the default opener is asserted without launching a
// real browser; the store still accepts an injectable opener for direct stubbing.
const openUrlMock = vi.fn();
vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: (url: string) => openUrlMock(url),
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

// C1: the local folder is chosen via the BACKEND-owned dialog command
// (pick_folder_dialog), which returns { path, token }. There is no longer a
// frontend tauri-plugin-dialog call in the wizard, so the dialog seam is the
// invoke mock itself (pick_folder_dialog below).

import { i18n } from "../i18n";
import SetupWizard from "../views/SetupWizard.vue";
import CredentialsWalkthrough from "../components/CredentialsWalkthrough.vue";
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
  pendingRecoveryAck: false,
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
      case "pick_folder_dialog":
        return Promise.resolve({ path: "/home/user/Docs", token: "tok-folder" });
      case "pick_drive_folder":
        return Promise.resolve(FAKE_DRIVE_LISTING);
      case "add_source":
        // B3: an encrypted add returns the one-time recovery phrase. M9c D4: the
        // first encrypted source is persisted DISABLED + pending a backend ack.
        return Promise.resolve({
          source: FAKE_SOURCE,
          recoveryPhrase: ["alpha", "bravo", "charlie"],
          pendingRecoveryAck: true,
        });
      case "reveal_recovery_phrase":
        // M9c D4: the backend reveal returns the same words + records the reveal.
        return Promise.resolve(["alpha", "bravo", "charlie"]);
      case "ack_recovery_phrase_saved":
        // M9c D4: the ack enables the (until-now disabled) source.
        return Promise.resolve({ ...FAKE_SOURCE, enabled: true });
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
      ].includes(cmd)
    );
}

beforeEach(() => {
  invokeMock.mockReset();
  unlistenMock.mockReset();
  oauthCompleteHandler = null;
  setActivePinia(createPinia());
  installFakeBackend();
  // The opener plugin is the browser-open seam; default it to resolve so the
  // default sign-in path "opens" without launching a real browser.
  openUrlMock.mockReset();
  openUrlMock.mockResolvedValue(undefined);
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
    // The auth URL is captured so the manual fallback can re-open / copy it.
    expect(setup.authUrl).toBe("https://accounts.google.com/o/x");
  });

  it("connectAccount uses the Tauri opener plugin (not window.open) by default", async () => {
    // The sign-in bug was window.open not reaching the system browser. With no
    // opener injected, the store must route through the opener plugin's openUrl.
    const setup = useSetupStore();
    await setup.begin();
    await setup.connectAccount("cid", "csec");
    expect(openUrlMock).toHaveBeenCalledWith("https://accounts.google.com/o/x");
    expect(setup.oauthStatus).toEqual({ kind: "awaitingCallback" });
  });

  it("connectAccount records browser_open_failed when the opener throws (not a dead end)", async () => {
    // A blocked auto-open must NOT wedge the wizard: the loopback server is still
    // listening, the auth URL stays captured, and a clear error code tells the
    // user to use the manual link. connectAccount must not reject in this case.
    const setup = useSetupStore();
    const failingOpener = vi.fn().mockRejectedValue(new Error("no browser"));
    await setup.begin();
    await expect(setup.connectAccount("cid", "csec", failingOpener)).resolves.toBeUndefined();
    expect(setup.errorCode).toBe("auth.browser_open_failed");
    // Still awaiting the callback, with the URL available for the fallback.
    expect(setup.oauthStatus).toEqual({ kind: "awaitingCallback" });
    expect(setup.authUrl).toBe("https://accounts.google.com/o/x");
  });

  it("openAuthUrl re-opens the captured auth URL via the injected opener", async () => {
    const setup = useSetupStore();
    await setup.begin();
    await setup.connectAccount("cid", "csec");
    // Simulate the first open failing so an error is showing.
    setup.errorCode = "auth.browser_open_failed";
    const opener = vi.fn();
    await setup.openAuthUrl(opener);
    expect(opener).toHaveBeenCalledWith("https://accounts.google.com/o/x");
    // A successful re-open clears the prior error.
    expect(setup.errorCode).toBeNull();
  });

  it("openAuthUrl records browser_open_failed when the manual re-open also fails", async () => {
    const setup = useSetupStore();
    await setup.begin();
    await setup.connectAccount("cid", "csec");
    const failingOpener = vi.fn().mockRejectedValue(new Error("still blocked"));
    await setup.openAuthUrl(failingOpener);
    expect(setup.errorCode).toBe("auth.browser_open_failed");
  });

  it("createFirstSource adds the source with its encryption flag + captures the phrase", async () => {
    const setup = useSetupStore();
    setup.accountId = "acct-1";
    setup.localPath = "/home/user/Docs";
    setup.localPathToken = "tok-folder";
    setup.driveFolderId = "drive-folder-1";
    setup.driveFolderPath = "/Backups/Docs";
    setup.encryptionEnabled = true;
    await setup.createFirstSource();

    const addCall = invokeMock.mock.calls.find((c) => c[0] === "add_source");
    expect(addCall).toBeDefined();
    const sentReq = (
      addCall![1] as {
        req: { encryptionEnabled: boolean; localPathToken: string };
      }
    ).req;
    expect(sentReq.encryptionEnabled).toBe(true);
    // C1: the dialog token rides along so the backend can prove the path.
    expect(sentReq.localPathToken).toBe("tok-folder");
    expect(setup.sourceId).toBe("src-1");
    // B3 + R3-P1-1: the one-time recovery phrase was captured + Finish is gated
    // until the phrase is REVEALED and acknowledged.
    expect(setup.recoveryPhrase).toEqual(["alpha", "bravo", "charlie"]);
    expect(setup.hasRecoveryPhrase).toBe(true);
    expect(setup.canFinish).toBe(false);
    // Acknowledging WITHOUT revealing must NOT enable Finish (the regression).
    setup.acknowledgePhrase(true);
    expect(setup.canFinish).toBe(false);
    // Revealing then acknowledging enables Finish.
    setup.markPhraseRevealed(true);
    expect(setup.canFinish).toBe(true);
    // Re-locking (phrase changed) clears both reveal + ack -> Finish disabled.
    setup.markPhraseRevealed(false);
    expect(setup.phraseAcknowledged).toBe(false);
    expect(setup.canFinish).toBe(false);
    // Reuses the sources store list (refresh fired after add).
    const sources = useSourcesStore();
    expect(sources.sources).toHaveLength(1);
  });

  it("createFirstSource is idempotent - re-entry does not re-call add_source (R1-P2-3)", async () => {
    // The one-shot folder dialog token is CONSUMED by the backend on the first
    // add_source. Re-entering the encryption step (Back from confirm, then Next
    // again) must NOT re-call add_source - it would fail with a stale token and
    // wedge the wizard. Assert the second createFirstSource is a no-op.
    const setup = useSetupStore();
    setup.accountId = "acct-1";
    setup.localPath = "/home/user/Docs";
    setup.localPathToken = "tok-folder";
    setup.driveFolderId = "drive-folder-1";
    setup.driveFolderPath = "/Backups/Docs";
    setup.encryptionEnabled = true;

    await setup.createFirstSource();
    expect(setup.sourceId).toBe("src-1");
    const addCallsAfterFirst = invokeMock.mock.calls.filter((c) => c[0] === "add_source").length;
    expect(addCallsAfterFirst).toBe(1);

    // Re-enter the step: createFirstSource must short-circuit (no second add).
    await setup.createFirstSource();
    const addCallsAfterSecond = invokeMock.mock.calls.filter((c) => c[0] === "add_source").length;
    expect(addCallsAfterSecond).toBe(1);
    expect(setup.errorCode).toBeNull();
    expect(setup.sourceId).toBe("src-1");
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
      (b) => b.text() === i18n.global.t("wizard.step2.signInButton")
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

    // Drive destination: the shared DriveFolderPicker auto-loads My Drive root on
    // mount and selects it (no button click needed - that missing feedback was the
    // bug). The fake backend returns drive-folder-1 as the current folder id.
    await flushPromises();
    expect(setup.driveFolderId).toBe("drive-folder-1");
    // The picker surfaces the chosen destination instead of a dead button.
    expect(wrapper.find('[data-testid="drive-folder-picker"]').exists()).toBe(true);

    await wrapper.get("footer button:last-child").trigger("click");
    expect(setup.step).toBe("encryption");

    // Step 4: opt into encryption, then advance (creates the source + returns
    // the one-time recovery phrase).
    const encBox = wrapper.find('input[type="checkbox"]');
    await encBox.setValue(true);
    expect(setup.encryptionEnabled).toBe(true);
    await wrapper.get("footer button:last-child").trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith(
      "add_source",
      expect.objectContaining({
        req: expect.objectContaining({
          encryptionEnabled: true,
          localPathToken: "tok-folder",
        }),
      })
    );
    expect(setup.step).toBe("confirm");

    // B3 + R3-P1-1: the recovery phrase is now displayed; Finish is GATED until
    // the user REVEALS and acknowledges they saved it. The Finish button must
    // start disabled.
    expect(setup.hasRecoveryPhrase).toBe(true);
    const finishBtn = () => wrapper.get("footer button:last-child");
    expect(finishBtn().attributes("disabled")).toBeDefined();

    // R3-P1-1: the acknowledge checkbox is DISABLED until the phrase is revealed,
    // so a user cannot confirm "I saved it" while it is still hidden.
    const ackBox = () => wrapper.get('[data-testid="phrase-ack"]');
    expect(ackBox().attributes("disabled")).toBeDefined();
    expect(setup.phraseRevealed).toBe(false);

    // Reveal the phrase (the first button in the reveal component).
    const revealBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    expect(revealBtn).toBeTruthy();
    await revealBtn!.trigger("click");
    await flushPromises();
    expect(setup.phraseRevealed).toBe(true);

    // Now the checkbox is enabled; acknowledge and Finish becomes enabled.
    expect(ackBox().attributes("disabled")).toBeUndefined();
    await ackBox().setValue(true);
    await flushPromises();
    expect(setup.phraseAcknowledged).toBe(true);
    expect(setup.canFinish).toBe(true);
    expect(finishBtn().attributes("disabled")).toBeUndefined();

    // Step 5: finish -> initial sync -> navigate to /activity.
    await finishBtn().trigger("click");
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

describe("CredentialsWalkthrough empty-secret (R1-P2-4, DESIGN s6.1)", () => {
  it("allows submit with a client ID and an EMPTY client secret", async () => {
    // A PKCE installed-app client legitimately has no secret. The sign-in button
    // must enable on a non-empty client ID ALONE, and submitting must pass the
    // (empty) secret straight through to the backend.
    installFakeBackend();
    const wrapper = mount(CredentialsWalkthrough, {
      global: { plugins: [i18n] },
    });
    await flushPromises();

    const inputs = wrapper.findAll("input");
    // Client ID only; leave the secret EMPTY.
    await inputs[0].setValue("my-installed-app-client-id");
    await flushPromises();

    const signInBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("wizard.step2.signInButton"));
    expect(signInBtn).toBeTruthy();
    // R1-P2-4: enabled despite the empty secret.
    expect(signInBtn!.attributes("disabled")).toBeUndefined();

    await signInBtn!.trigger("click");
    await flushPromises();

    // The empty secret was forwarded as-is (trimmed empty string).
    expect(invokeMock).toHaveBeenCalledWith("submit_oauth_credentials", {
      session: FAKE_SESSION,
      clientId: "my-installed-app-client-id",
      clientSecret: "",
    });
  });

  it("still blocks submit when the client ID is empty", async () => {
    installFakeBackend();
    const wrapper = mount(CredentialsWalkthrough, {
      global: { plugins: [i18n] },
    });
    await flushPromises();
    const inputs = wrapper.findAll("input");
    // Secret present but NO client ID -> still blocked.
    await inputs[1].setValue("some-secret");
    await flushPromises();
    const signInBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("wizard.step2.signInButton"));
    expect(signInBtn!.attributes("disabled")).toBeDefined();
  });
});
