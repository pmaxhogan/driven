// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";
import { createRouter, createMemoryHistory } from "vue-router";

// The SSH (SFTP) destination's wizard path. Like the local-folder path it has
// no OAuth consent flow, so step 2 branches to yet another component - but
// UNLIKE local-folder, SFTP DOES browse (readdir over SFTP), so step 3 must
// still show the DriveFolderPicker, not the fixed-destination paragraph. This
// is also the regression suite for the step-2 branch itself: before this
// change, the `v-if oauth / v-else-if local / v-else s3` binary meant a THIRD
// non-OAuth destination silently fell into the S3 arm and submitted the wrong
// request shape - so every test here that mounts step 2 asserts on the
// component actually rendered, not just on "a form exists".

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
import SshCredentialsForm from "../components/SshCredentialsForm.vue";
import S3CredentialsForm from "../components/S3CredentialsForm.vue";
import LocalFolderForm from "../components/LocalFolderForm.vue";
import CredentialsWalkthrough from "../components/CredentialsWalkthrough.vue";
import DriveFolderPicker from "../components/DriveFolderPicker.vue";
import { useSetupStore } from "../stores/setup";
import type { CreateSftpAccountRequest } from "../ipc/types";

const DRIVE_BACKEND = {
  id: "google_drive",
  usesOauth: true,
  supportsFolderPicker: true,
  supportsVersionHistory: true,
  isDefault: true,
};
const S3_BACKEND = {
  id: "s3",
  usesOauth: false,
  supportsFolderPicker: true,
  supportsVersionHistory: true,
  isDefault: false,
};
// SFTP browses (readdir over SFTP, DESIGN "SSH (SFTP) backend" s2), so unlike
// the local folder it DOES support the folder picker.
const SFTP_BACKEND = {
  id: "sftp",
  usesOauth: false,
  supportsFolderPicker: true,
  supportsVersionHistory: false,
  isDefault: false,
};
const FAKE_ACCOUNT = {
  id: "acct-sftp-1",
  email: "driven@nas.example.com:/backups/driven",
  displayName: null,
  state: "ok",
  encryptionEnabled: false,
  createdAt: 0,
  lastSyncedAt: null,
  backendKind: "sftp",
};
const FINGERPRINT = "SHA256:abcDEF0123456789abcDEF0123456789abcDEF012";
const FAKE_S3_ACCOUNT = {
  id: "acct-s3-1",
  email: "my-backups/",
  displayName: null,
  state: "ok",
  encryptionEnabled: false,
  createdAt: 0,
  lastSyncedAt: null,
  backendKind: "s3",
};

const VALID_REQ: CreateSftpAccountRequest = {
  host: "nas.example.com",
  port: null,
  rootPath: "/backups/driven",
  username: "driven",
  auth: "password",
  password: "hunter2",
  privateKey: null,
  passphrase: null,
};

function installFakeBackend(opts: { adopted?: boolean } = {}): void {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "list_backends":
        return Promise.resolve([DRIVE_BACKEND, S3_BACKEND, SFTP_BACKEND]);
      case "create_sftp_account":
        return Promise.resolve({
          account: FAKE_ACCOUNT,
          hostKeyFingerprint: FINGERPRINT,
          adopted: opts.adopted ?? false,
        });
      case "create_s3_account":
        return Promise.resolve(FAKE_S3_ACCOUNT);
      case "begin_add_account_wizard":
        return Promise.resolve("sess-1");
      case "pick_drive_folder":
        return Promise.resolve({ currentFolderId: "", currentFolderPath: "", folders: [] });
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
});

describe("setup store: createSftpAccount", () => {
  beforeEach(() => installFakeBackend());

  it("unwraps .account, records the fingerprint, and resolves the sign-in gate", async () => {
    const setup = useSetupStore();
    const ok = await setup.createSftpAccount(VALID_REQ);

    expect(ok).toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("create_sftp_account", { req: VALID_REQ });
    expect(setup.accountId).toBe("acct-sftp-1");
    expect(setup.accountEmail).toBe("driven@nas.example.com:/backups/driven");
    expect(setup.sftpHostKeyFingerprint).toBe(FINGERPRINT);
    expect(setup.sftpAdopted).toBe(false);
    // There is no consent round trip, so the source step's gate has to be
    // resolved here or the wizard would never advance.
    expect(setup.signedIn).toBe(true);
    expect(setup.busy).toBe(false);
  });

  it("records adopted:true when the probe reused an existing destination", async () => {
    installFakeBackend({ adopted: true });
    const setup = useSetupStore();
    await setup.createSftpAccount(VALID_REQ);
    expect(setup.sftpAdopted).toBe(true);
  });

  it("surfaces a rejection as an error code and does not advance", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "create_sftp_account") {
        return Promise.reject({ code: "auth.invalid_grant", message: "nope" });
      }
      return Promise.resolve(undefined);
    });
    const setup = useSetupStore();
    const ok = await setup.createSftpAccount(VALID_REQ);

    expect(ok).toBe(false);
    expect(setup.errorCode).toBe("auth.invalid_grant");
    expect(setup.accountId).toBeNull();
    expect(setup.sftpHostKeyFingerprint).toBeNull();
    expect(setup.signedIn).toBe(false);
    expect(setup.busy).toBe(false);
  });

  it("clears the fingerprint and adopted flag on reset", async () => {
    const setup = useSetupStore();
    await setup.createSftpAccount(VALID_REQ);
    expect(setup.sftpHostKeyFingerprint).toBe(FINGERPRINT);
    setup.reset();
    expect(setup.sftpHostKeyFingerprint).toBeNull();
    expect(setup.sftpAdopted).toBe(false);
  });
});

describe("setup wizard: SSH (SFTP) branch", () => {
  async function mountWizard() {
    const r = router();
    r.push("/setup");
    await r.isReady();
    const wrapper = mount(SetupWizard, { global: { plugins: [i18n, r] } });
    await flushPromises();
    return wrapper;
  }

  it("shows the SSH form instead of the OAuth walkthrough or the local-folder form", async () => {
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();

    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();

    expect(wrapper.findComponent(SshCredentialsForm).exists()).toBe(true);
    expect(wrapper.findComponent(CredentialsWalkthrough).exists()).toBe(false);
    expect(wrapper.findComponent(LocalFolderForm).exists()).toBe(false);
    expect(wrapper.findComponent(S3CredentialsForm).exists()).toBe(false);
  });

  it("REGRESSION: an S3 backend still hits the S3 branch, not SSH", async () => {
    // The whole point of making step 2 explicit: a third non-OAuth backend
    // (SFTP) must not turn the old `v-else` into an accidental catch-all that
    // swallows S3 too.
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();

    setup.selectBackend("s3");
    setup.next();
    await flushPromises();

    expect(wrapper.findComponent(S3CredentialsForm).exists()).toBe(true);
    expect(wrapper.findComponent(SshCredentialsForm).exists()).toBe(false);
  });

  it("advances to the source step once the server is connected, and shows the pinned fingerprint", async () => {
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();

    const form = wrapper.findComponent(SshCredentialsForm);
    form.vm.$emit("submit", VALID_REQ);
    await flushPromises();

    expect(setup.step).toBe("source");
    expect(setup.accountId).toBe("acct-sftp-1");
    const confirmation = wrapper.get('[data-testid="sftp-fingerprint-confirmation"]');
    expect(confirmation.text()).toContain(FINGERPRINT);
    expect(wrapper.find('[data-testid="sftp-adopted-note"]').exists()).toBe(false);
  });

  it("REGRESSION: does not show a stale SFTP fingerprint after abandoning SFTP for S3", async () => {
    // Create an SFTP account, go Back, then create an S3 account instead. The
    // fingerprint/adopted note is keyed off `sftpHostKeyFingerprint` alone
    // being set, which survives a Back - without a backendId gate (and the
    // store clearing it on a DIFFERENT backend's successful create), the S3
    // account's source step would render SFTP's TOFU confirmation for a
    // destination it has nothing to do with.
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();

    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();
    wrapper.findComponent(SshCredentialsForm).vm.$emit("submit", VALID_REQ);
    await flushPromises();
    expect(setup.step).toBe("source");
    expect(setup.sftpHostKeyFingerprint).toBe(FINGERPRINT);

    // back() from "source" lands on "credentials" directly - no second next()
    // needed (or wanted: it would skip the S3 form entirely).
    setup.back();
    expect(setup.step).toBe("credentials");
    setup.selectBackend("s3");
    await flushPromises();

    const s3Req = {
      endpoint: "https://example.r2.cloudflarestorage.com",
      bucket: "my-backups",
      region: null,
      prefix: null,
      pathStyle: true,
      accessKeyId: "AKIAEXAMPLE",
      secretAccessKey: "super-secret",
    };
    wrapper.findComponent(S3CredentialsForm).vm.$emit("submit", s3Req);
    await flushPromises();

    expect(setup.step).toBe("source");
    expect(setup.accountId).toBe("acct-s3-1");
    // The store itself no longer carries the abandoned attempt's fingerprint -
    // not just the template's backendId gate hiding it.
    expect(setup.sftpHostKeyFingerprint).toBeNull();
    expect(setup.sftpAdopted).toBe(false);
    expect(wrapper.find('[data-testid="sftp-fingerprint-confirmation"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="sftp-adopted-note"]').exists()).toBe(false);
  });

  it("shows the adopted note when the probe reused an existing destination", async () => {
    installFakeBackend({ adopted: true });
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();

    wrapper.findComponent(SshCredentialsForm).vm.$emit("submit", VALID_REQ);
    await flushPromises();

    expect(wrapper.find('[data-testid="sftp-adopted-note"]').exists()).toBe(true);
  });

  it("still offers the folder picker on step 3, since SFTP browses its destination", async () => {
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();
    wrapper.findComponent(SshCredentialsForm).vm.$emit("submit", VALID_REQ);
    await flushPromises();

    expect(wrapper.findComponent(DriveFolderPicker).exists()).toBe(true);
    expect(wrapper.find('[data-testid="local-destination-path"]').exists()).toBe(false);
  });

  it("stays on the form and renders a distinct, actionable message when the root path is missing", async () => {
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "create_sftp_account") {
        return Promise.reject({
          code: "sftp.root_missing",
          message: "sftp.root_missing: the configured root path /nope does not exist",
        });
      }
      return Promise.resolve(undefined);
    });

    wrapper.findComponent(SshCredentialsForm).vm.$emit("submit", VALID_REQ);
    await flushPromises();

    expect(setup.step).toBe("credentials");
    expect(setup.errorCode).toBe("sftp.root_missing");
    // A distinct, actionable message - not the generic "something went wrong"
    // internal.bug fallback, and not a raw echo of the backend's message.
    const shown = wrapper.text();
    expect(shown).toContain(i18n.global.t("errors.sftp.root_missing.long"));
    expect(shown).not.toContain(i18n.global.t("errors.internal.bug.long"));
  });

  it("stays on the form and renders a distinct message when the root points at a file", async () => {
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "create_sftp_account") {
        return Promise.reject({
          code: "sftp.root_not_a_directory",
          message: "sftp.root_not_a_directory: /backups/driven exists but is not a directory",
        });
      }
      return Promise.resolve(undefined);
    });

    wrapper.findComponent(SshCredentialsForm).vm.$emit("submit", VALID_REQ);
    await flushPromises();

    expect(setup.step).toBe("credentials");
    expect(setup.errorCode).toBe("sftp.root_not_a_directory");
    const shown = wrapper.text();
    expect(shown).toContain(i18n.global.t("errors.sftp.root_not_a_directory.long"));
    expect(shown).not.toContain(i18n.global.t("errors.internal.bug.long"));
  });

  it("stays on the form and renders a distinct message when the root already holds a different destination", async () => {
    // The backend cannot actually produce sftp.dest_marker_mismatch through
    // create_sftp_account today (see the Rust-side unit test on the mapper
    // for why: a fresh account's config never carries a pre-set destination
    // id). This still proves the WIZARD side of the contract - if that code
    // ever arrives (a future reauth/re-probe path), the UI already renders it
    // distinctly rather than falling back to internal.bug.
    installFakeBackend();
    const wrapper = await mountWizard();
    const setup = useSetupStore();
    setup.selectBackend("sftp");
    setup.next();
    await flushPromises();

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "create_sftp_account") {
        return Promise.reject({
          code: "sftp.dest_marker_mismatch",
          message: "sftp.dest_marker_mismatch: /backups/driven holds a different destination",
        });
      }
      return Promise.resolve(undefined);
    });

    wrapper.findComponent(SshCredentialsForm).vm.$emit("submit", VALID_REQ);
    await flushPromises();

    expect(setup.step).toBe("credentials");
    expect(setup.errorCode).toBe("sftp.dest_marker_mismatch");
    const shown = wrapper.text();
    expect(shown).toContain(i18n.global.t("errors.sftp.dest_marker_mismatch.long"));
    expect(shown).not.toContain(i18n.global.t("errors.internal.bug.long"));
  });
});
