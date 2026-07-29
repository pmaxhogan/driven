// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

// Regression tests for the destination step of BOTH wizards, driven as a
// NON-Drive account.
//
// The release-blocking bug these lock down: both wizards hard-coded the Google
// Drive shape. First-run setup rendered the Drive folder picker for every
// destination and gated Next on `!!driveFolderId`; "Add source" carried a static
// step list containing a "Drive folder" step. The whole 566-test suite passed
// while an S3 account could not complete setup at all, because nothing ever drove
// either wizard as anything but a Drive account - so these tests do exactly that.
//
// The specific trap is the EMPTY STRING. `picker_root_id(S3)` is "" (a bucket root
// IS the empty key prefix), so the one destination an S3 account starts on is
// falsy, and `!!driveFolderId` rejected it. Every assertion below that involves an
// S3 root deliberately uses "" rather than a convenient non-empty prefix - a test
// that only ever picks a sub-prefix would pass against the broken code.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => undefined),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn(),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(),
}));
const pushMock = vi.fn();
vi.mock("vue-router", () => ({
  useRouter: () => ({ push: pushMock }),
  useRoute: () => ({ params: {} }),
}));

import { i18n } from "../i18n";
import AddSourceWizard from "../components/AddSourceWizard.vue";
import SetupWizard from "../views/SetupWizard.vue";
import { useSetupStore } from "../stores/setup";
import type { AccountDto, BackendDto, BackendKindId } from "../ipc/types";

const globalMountOptions = { plugins: [i18n] };

/** The descriptors the Rust factory reports. `google_drive` and `s3` both browse
 * (Drive lists folders, S3 lists key prefixes); `local_folder` does not - its
 * destination root is fixed when the account is created. */
const BACKENDS: BackendDto[] = [
  { id: "google_drive", usesOauth: true, supportsFolderPicker: true, isDefault: true },
  { id: "s3", usesOauth: false, supportsFolderPicker: true, isDefault: false },
  { id: "local_folder", usesOauth: false, supportsFolderPicker: false, isDefault: false },
];

function makeAccount(backendKind: BackendKindId, email: string): AccountDto {
  return {
    id: "acc-1",
    email,
    displayName: null,
    state: "ok",
    encryptionEnabled: false,
    createdAt: 0,
    lastSyncedAt: null,
    backendKind,
  };
}

/**
 * Fake backend for the add-source wizard.
 *
 * `rootFolderId` is what `pick_drive_folder` echoes for the destination root -
 * "" for a bucket root, "root" for My Drive.
 */
function installBackend(opts: {
  account: AccountDto;
  rootFolderId: string;
  backends?: BackendDto[];
}): void {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "list_backends":
        return Promise.resolve(opts.backends ?? BACKENDS);
      case "list_accounts":
        return Promise.resolve([opts.account]);
      case "pick_drive_folder":
        return Promise.resolve({
          currentFolderId: opts.rootFolderId,
          currentFolderPath: "",
          folders: [],
        });
      case "pick_folder_dialog":
        return Promise.resolve({ path: "/home/u/docs", token: "tok-folder" });
      case "preview_exclusions_start":
        return Promise.resolve("gen-1");
      case "add_source":
        return Promise.resolve({
          source: {
            id: "src-new",
            accountId: opts.account.id,
            displayName: "docs",
            enabled: true,
            localPath: "/home/u/docs",
            driveFolderId: opts.rootFolderId,
            driveFolderPath: "",
            encryptionEnabled: false,
            respectGitignore: true,
            includePatterns: [],
            excludePatterns: [],
            placeholderPolicy: "skip",
            deepVerifyIntervalSecs: 604800,
            lastFullScanAt: null,
            createdAt: 0,
            pendingRecoveryAck: false,
          },
          recoveryPhrase: null,
          pendingRecoveryAck: false,
        });
      case "list_sources":
        return Promise.resolve([]);
      default:
        return Promise.resolve(undefined);
    }
  });
}

/** The visible step labels in the add-source wizard's breadcrumb. */
function stepLabels(wrapper: ReturnType<typeof mount>): string[] {
  return wrapper.findAll("ol li").map((li) => li.text());
}

beforeEach(() => {
  invokeMock.mockReset();
  pushMock.mockReset();
  setActivePinia(createPinia());
});

describe("first-run setup wizard: destination step per backend", () => {
  /** Walk the setup store to the source step as an already-connected account of
   * `backendKind`, then mount the wizard on that step. */
  async function mountOnSourceStep(backendKind: BackendKindId, accountEmail: string) {
    const wrapper = mount(SetupWizard, { global: globalMountOptions });
    await flushPromises();
    const setup = useSetupStore();
    setup.selectBackend(backendKind);
    setup.accountId = "acc-1";
    setup.accountEmail = accountEmail;
    // The credentials step resolved (OAuth consent, or an S3 key pair accepted).
    setup.oauthStatus = { kind: "complete" };
    setup.stepIndex = 2; // welcome, credentials, source
    await flushPromises();
    return { wrapper, setup };
  }

  function nextButton(wrapper: ReturnType<typeof mount>) {
    return wrapper.findAll("button").find((b) => b.text() === i18n.global.t("common.next"));
  }

  it("enables Next for an S3 account whose destination is the BUCKET ROOT (empty id)", async () => {
    // THE regression. The bucket root's id is "", which `!!driveFolderId`
    // rejected - so Next stayed disabled forever and setup could not finish.
    installBackend({
      account: makeAccount("s3", "my-bucket/backups/"),
      rootFolderId: "",
    });
    const { wrapper, setup } = await mountOnSourceStep("s3", "my-bucket/backups/");

    // A browsable destination still gets the picker: S3 prefixes are browsable.
    expect(wrapper.find('[data-testid="drive-folder-picker"]').exists()).toBe(true);

    // The local folder is chosen, and the picker settled on the bucket root.
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("wizard.step3.chooseFolderButton"))!
      .trigger("click");
    await flushPromises();
    expect(setup.driveFolderId).toBe("");

    expect(setup.destinationSelected).toBe(true);
    expect(nextButton(wrapper)!.attributes("disabled")).toBeUndefined();
  });

  it("keeps Next disabled until a destination is settled", async () => {
    // The gate must still GATE: with no local folder picked, Next stays disabled.
    installBackend({ account: makeAccount("s3", "my-bucket/"), rootFolderId: "" });
    const { wrapper, setup } = await mountOnSourceStep("s3", "my-bucket/");
    expect(setup.localPathToken).toBeNull();
    expect(nextButton(wrapper)!.attributes("disabled")).toBeDefined();
  });

  it("shows the Drive folder picker for a Drive account and gates on a real id", async () => {
    installBackend({
      account: makeAccount("google_drive", "user@example.com"),
      rootFolderId: "root",
    });
    const { wrapper, setup } = await mountOnSourceStep("google_drive", "user@example.com");
    expect(wrapper.find('[data-testid="drive-folder-picker"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="fixed-destination"]').exists()).toBe(false);
    await flushPromises();
    expect(setup.driveFolderId).toBe("root");
  });

  it("replaces the picker with the account's fixed destination when it cannot be browsed", async () => {
    // A local / removable folder has no browsable tree: its root was chosen when
    // the account was created. Rendering a remote picker there would show an
    // empty box above a dead Next, so the step shows the destination instead and
    // Next depends only on the local folder.
    installBackend({
      account: makeAccount("local_folder", "/Volumes/Backup"),
      rootFolderId: "",
    });
    const { wrapper, setup } = await mountOnSourceStep("local_folder", "/Volumes/Backup");
    setup.localFolderRoot = "/Volumes/Backup";
    await flushPromises();

    expect(wrapper.find('[data-testid="drive-folder-picker"]').exists()).toBe(false);
    const fixed = wrapper.get('[data-testid="local-destination-path"]');
    expect(fixed.text()).toContain("/Volumes/Backup");

    // Nothing to pick, so the destination is already satisfied...
    expect(setup.destinationSelected).toBe(true);
    // ...but Next still waits for the local folder.
    expect(nextButton(wrapper)!.attributes("disabled")).toBeDefined();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("wizard.step3.chooseFolderButton"))!
      .trigger("click");
    await flushPromises();
    expect(nextButton(wrapper)!.attributes("disabled")).toBeUndefined();
  });
});

describe("setup store: destination predicate and add_source payload", () => {
  it("sends the empty bucket-root id rather than refusing to create the source", async () => {
    installBackend({ account: makeAccount("s3", "my-bucket/"), rootFolderId: "" });
    const setup = useSetupStore();
    await setup.loadBackends();
    setup.selectBackend("s3");
    setup.accountId = "acc-1";
    setup.localPath = "/home/u/docs";
    setup.localPathToken = "tok-folder";
    setup.driveFolderId = "";
    setup.driveFolderPath = "";

    await setup.createFirstSource();

    const addCall = invokeMock.mock.calls.find((c) => c[0] === "add_source");
    expect(addCall).toBeDefined();
    const req = (addCall![1] as { req: { driveFolderId: string } }).req;
    expect(req.driveFolderId).toBe("");
  });

  it("still refuses when no destination has been settled on a browsable backend", async () => {
    installBackend({ account: makeAccount("s3", "my-bucket/"), rootFolderId: "" });
    const setup = useSetupStore();
    await setup.loadBackends();
    setup.selectBackend("s3");
    setup.accountId = "acc-1";
    setup.localPath = "/home/u/docs";
    setup.localPathToken = "tok-folder";
    // The picker never resolved, so no destination exists - null, not "".
    setup.driveFolderId = null;

    expect(setup.destinationSelected).toBe(false);
    await expect(setup.createFirstSource()).rejects.toThrow();
    expect(invokeMock.mock.calls.some((c) => c[0] === "add_source")).toBe(false);
  });

  it("treats a backend with no browsable tree as already having its destination", async () => {
    installBackend({ account: makeAccount("local_folder", "/Volumes/Backup"), rootFolderId: "" });
    const setup = useSetupStore();
    await setup.loadBackends();
    setup.selectBackend("local_folder");
    expect(setup.backendSupportsFolderPicker).toBe(false);
    expect(setup.driveFolderId).toBeNull();
    expect(setup.destinationSelected).toBe(true);
  });
});

describe("add-source wizard: step list per backend", () => {
  async function openWizard() {
    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();
    return wrapper;
  }

  it("keeps the destination step for an S3 account and accepts the bucket root", async () => {
    installBackend({ account: makeAccount("s3", "my-bucket/backups/"), rootFolderId: "" });
    const wrapper = await openWizard();

    expect(stepLabels(wrapper)).toContain(i18n.global.t("settings.addSource.step.driveFolder"));

    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"))!
      .trigger("click");
    await flushPromises();
    const next = () =>
      wrapper.findAll("button").find((b) => b.text() === i18n.global.t("common.next"))!;
    await next().trigger("click");
    await flushPromises();

    // On the destination step, browsing an S3 bucket root yields "" - which must
    // enable Next, not disable it.
    expect(wrapper.find('[data-testid="drive-folder-picker"]').exists()).toBe(true);
    expect(next().attributes("disabled")).toBeUndefined();
  });

  it("drops the destination step for an account whose destination cannot be browsed", async () => {
    // The static step list showed "Drive folder" to every account, including ones
    // with no remote tree to browse, walking them into an empty step.
    installBackend({ account: makeAccount("local_folder", "/Volumes/Backup"), rootFolderId: "" });
    const wrapper = await openWizard();

    expect(stepLabels(wrapper)).not.toContain(i18n.global.t("settings.addSource.step.driveFolder"));
    expect(stepLabels(wrapper)).toEqual([
      i18n.global.t("settings.addSource.step.localFolder"),
      i18n.global.t("settings.addSource.step.exclusions"),
      i18n.global.t("settings.addSource.step.encryption"),
      i18n.global.t("settings.addSource.step.confirm"),
    ]);

    // Advancing goes straight from the local folder to the exclusions preview,
    // and the picker is never mounted at all.
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.addSource.chooseLocalButton"))!
      .trigger("click");
    await flushPromises();
    await wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.next"))!
      .trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="drive-folder-picker"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="exclusion-preview"]').exists()).toBe(true);
    expect(invokeMock.mock.calls.some((c) => c[0] === "pick_drive_folder")).toBe(false);
  });

  it("shows the account's own destination on the confirm step when there is nothing to browse", async () => {
    installBackend({ account: makeAccount("local_folder", "/Volumes/Backup"), rootFolderId: "" });
    const wrapper = await openWizard();
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
    await clickNext(); // -> exclusions
    await clickNext(); // -> encryption
    await clickNext(); // -> confirm

    expect(wrapper.get('[data-testid="confirm-destination"]').text()).toContain("/Volumes/Backup");
  });

  it("keeps the Drive step for a Drive account (no behaviour change)", async () => {
    installBackend({
      account: makeAccount("google_drive", "user@example.com"),
      rootFolderId: "root",
    });
    const wrapper = await openWizard();
    expect(stepLabels(wrapper)).toContain(i18n.global.t("settings.addSource.step.driveFolder"));
  });

  it("falls back to the browsable shape when the descriptors cannot be loaded", async () => {
    // `list_backends` failing must not hide a step the account needs - the wizard
    // degrades to the historical Drive behaviour rather than dead-ending.
    installBackend({
      account: makeAccount("google_drive", "user@example.com"),
      rootFolderId: "root",
    });
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_backends") return Promise.reject(new Error("nope"));
      if (cmd === "list_accounts")
        return Promise.resolve([makeAccount("google_drive", "user@example.com")]);
      if (cmd === "pick_drive_folder")
        return Promise.resolve({ currentFolderId: "root", currentFolderPath: "", folders: [] });
      return Promise.resolve(undefined);
    });
    const wrapper = await openWizard();
    expect(stepLabels(wrapper)).toContain(i18n.global.t("settings.addSource.step.driveFolder"));
  });
});
