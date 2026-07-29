// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";
import { createRouter, createMemoryHistory } from "vue-router";

// The local / removable-folder destination's wizard path. It is the first
// destination with NO OAuth consent flow, so step 2 branches to a different
// component and step 3 has no browsable remote tree - two forks the Drive path
// never exercises.
//
// Everything routes through the typed IPC wrappers, so mocking `invoke` is
// enough to walk the whole flow against a fake backend with no Tauri runtime.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(vi.fn()),
}));

import { i18n } from "../i18n";
import SetupWizard from "../views/SetupWizard.vue";
import LocalFolderForm from "../components/LocalFolderForm.vue";
import CredentialsWalkthrough from "../components/CredentialsWalkthrough.vue";
import DriveFolderPicker from "../components/DriveFolderPicker.vue";
import { useSetupStore } from "../stores/setup";

const DRIVE_BACKEND = {
  id: "google_drive",
  usesOauth: true,
  supportsFolderPicker: true,
  isDefault: true,
};
const LOCAL_BACKEND = {
  id: "local_folder",
  usesOauth: false,
  supportsFolderPicker: false,
  isDefault: false,
};
const FAKE_ACCOUNT = {
  id: "acct-local-1",
  email: "Backup",
  displayName: null,
  state: "ok",
  encryptionEnabled: false,
  createdAt: 0,
  lastSyncedAt: null,
  backendKind: "local_folder",
};

function installFakeBackend(): void {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "list_backends":
        return Promise.resolve([DRIVE_BACKEND, LOCAL_BACKEND]);
      case "create_local_folder_account":
        return Promise.resolve(FAKE_ACCOUNT);
      case "begin_add_account_wizard":
        return Promise.resolve("sess-1");
      case "pick_folder_dialog":
        return Promise.resolve({ path: "/Volumes/Backup", token: "tok-1" });
      default:
        return Promise.resolve(undefined);
    }
  });
}

function router() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", component: { template: "<div />" } },
      { path: "/setup", component: SetupWizard },
    ],
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  setActivePinia(createPinia());
  installFakeBackend();
});

describe("local-folder destination store actions", () => {
  it("createLocalFolderAccount records the account and resolves the sign-in gate", async () => {
    const setup = useSetupStore();
    const ok = await setup.createLocalFolderAccount({
      root: "/Volumes/Backup",
      displayName: null,
    });

    expect(ok).toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("create_local_folder_account", {
      req: { root: "/Volumes/Backup", displayName: null },
    });
    expect(setup.accountId).toBe("acct-local-1");
    expect(setup.accountEmail).toBe("Backup");
    expect(setup.localFolderRoot).toBe("/Volumes/Backup");
    // There is no consent round trip, so the source step's gate has to be
    // resolved here or the wizard would never advance.
    expect(setup.signedIn).toBe(true);
    expect(setup.busy).toBe(false);
  });

  it("surfaces a rejected folder as an error code and does not advance", async () => {
    const setup = useSetupStore();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "create_local_folder_account") {
        return Promise.reject({ code: "localfs.dest_not_writable" });
      }
      return Promise.resolve(undefined);
    });

    const ok = await setup.createLocalFolderAccount({
      root: "/Volumes/ReadOnly",
      displayName: null,
    });

    expect(ok).toBe(false);
    expect(setup.accountId).toBeNull();
    expect(setup.localFolderRoot).toBeNull();
    expect(setup.errorCode).toBe("localfs.dest_not_writable");
    expect(setup.busy).toBe(false);
  });

  it("derives a whitespace-free destination sub-folder from the source path", () => {
    const setup = useSetupStore();
    // `add_source` rejects whitespace in a destination folder id, so a source
    // whose folder name has spaces must still produce a legal one.
    expect(setup.destinationFolderIdFor("/home/user/Documents")).toBe("Documents/");
    expect(setup.destinationFolderIdFor("/home/user/My Documents")).toBe("My-Documents/");
    expect(setup.destinationFolderIdFor("C:\\Users\\me\\Docs")).toBe("Docs/");
    expect(setup.destinationFolderIdFor("/home/user/Docs/")).toBe("Docs/");
    // A path with no component at all still yields something non-empty.
    expect(setup.destinationFolderIdFor("/")).toBe("backup/");
  });

  it("reports whether the chosen destination has a browsable tree", async () => {
    const setup = useSetupStore();
    await setup.loadBackends();

    setup.selectBackend("google_drive");
    expect(setup.backendUsesOauth).toBe(true);
    expect(setup.backendSupportsFolderPicker).toBe(true);

    setup.selectBackend("local_folder");
    expect(setup.backendUsesOauth).toBe(false);
    expect(setup.backendSupportsFolderPicker).toBe(false);
  });

  it("clears the local destination root on reset", async () => {
    const setup = useSetupStore();
    await setup.createLocalFolderAccount({ root: "/Volumes/Backup", displayName: null });
    expect(setup.localFolderRoot).toBe("/Volumes/Backup");
    setup.reset();
    expect(setup.localFolderRoot).toBeNull();
  });
});

describe("setup wizard, local-folder branch", () => {
  async function mountWizard() {
    const r = router();
    r.push("/setup");
    await r.isReady();
    const wrapper = mount(SetupWizard, { global: { plugins: [i18n, r] } });
    await flushPromises();
    return wrapper;
  }

  it("shows the folder form instead of the OAuth walkthrough on step 2", async () => {
    const wrapper = await mountWizard();
    const setup = useSetupStore();

    // Step 1: choose the local-folder destination.
    setup.selectBackend("local_folder");
    setup.next();
    await flushPromises();

    expect(wrapper.findComponent(LocalFolderForm).exists()).toBe(true);
    expect(wrapper.findComponent(CredentialsWalkthrough).exists()).toBe(false);
  });

  it("advances to the source step once the folder is accepted", async () => {
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("local_folder");
    setup.next();
    await flushPromises();

    const form = wrapper.findComponent(LocalFolderForm);
    form.vm.$emit("submit", { root: "/Volumes/Backup", displayName: null });
    await flushPromises();

    expect(setup.step).toBe("source");
    expect(setup.accountId).toBe("acct-local-1");
  });

  it("stays on the form when the backend rejects the folder", async () => {
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("local_folder");
    setup.next();
    await flushPromises();

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "create_local_folder_account") {
        return Promise.reject({ code: "localfs.dest_not_writable" });
      }
      return Promise.resolve(undefined);
    });

    wrapper
      .findComponent(LocalFolderForm)
      .vm.$emit("submit", { root: "/Volumes/ReadOnly", displayName: null });
    await flushPromises();

    expect(setup.step).toBe("credentials");
    expect(setup.errorCode).toBe("localfs.dest_not_writable");
  });

  it("replaces the Drive folder picker with the resolved destination path", async () => {
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("local_folder");
    setup.next();
    await flushPromises();

    wrapper
      .findComponent(LocalFolderForm)
      .vm.$emit("submit", { root: "/Volumes/Backup", displayName: null });
    await flushPromises();
    expect(setup.step).toBe("source");

    // No browsable remote tree for this destination. Before a source folder is
    // chosen there is no sub-folder yet, so the destination ROOT is shown.
    expect(wrapper.findComponent(DriveFolderPicker).exists()).toBe(false);
    const shown = wrapper.get('[data-testid="local-destination-path"]');
    expect(shown.text()).toBe("/Volumes/Backup");

    // Choosing the source folder derives the destination sub-folder and shows
    // the resolved path, so it is never a surprise.
    await wrapper.get('[data-testid="wizard-choose-folder"]').trigger("click");
    await flushPromises();

    expect(setup.driveFolderId).toBe("Backup/");
    expect(setup.driveFolderPath).toBe("/Volumes/Backup/Backup/");
    expect(wrapper.get('[data-testid="local-destination-path"]').text()).toBe(
      "/Volumes/Backup/Backup/"
    );
    // With both a source token and a destination, the step can advance - the
    // Drive path's gate would still be waiting on a folder id.
    expect(setup.localPathToken).toBe("tok-1");
  });
});
