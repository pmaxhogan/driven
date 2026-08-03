// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// The SSH (SFTP) destination's credentials step. Two auth modes share one
// form: a plain password, or a pasted private key with an optional
// passphrase - so beyond "does it render", what matters is that the emitted
// request carries exactly one credential shape (never both), and that a
// passphrase is only ever submitted alongside a private key.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { i18n } from "../i18n";
import SshCredentialsForm from "../components/SshCredentialsForm.vue";
import type { CreateSftpAccountRequest } from "../ipc/types";

function mountForm(props: Record<string, unknown> = {}) {
  return mount(SshCredentialsForm, {
    props,
    global: { plugins: [i18n] },
  });
}

async function fill(
  wrapper: ReturnType<typeof mountForm>,
  values: Partial<Record<string, string>>
) {
  for (const [id, value] of Object.entries(values)) {
    await wrapper.get(`#${id}`).setValue(value);
  }
}

const HOST_FIELDS = {
  "sftp-host": "nas.example.com",
  "sftp-root-path": "/backups/driven",
  "sftp-username": "driven",
};

describe("SshCredentialsForm", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setActivePinia(createPinia());
  });

  it("defaults to password auth and keeps submit disabled until every required field is filled", async () => {
    const wrapper = mountForm();
    const button = wrapper.get('[data-testid="sftp-connect"]');
    expect((button.element as HTMLButtonElement).disabled).toBe(true);

    await fill(wrapper, HOST_FIELDS);
    expect((button.element as HTMLButtonElement).disabled).toBe(true);

    await wrapper.get("#sftp-password").setValue("hunter2");
    expect((button.element as HTMLButtonElement).disabled).toBe(false);
  });

  it("emits a password-auth request with no private-key fields and a null optional port", async () => {
    const wrapper = mountForm();
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get("#sftp-password").setValue("hunter2");
    await wrapper.get("form").trigger("submit");

    const emitted = wrapper.emitted("submit");
    expect(emitted).toHaveLength(1);
    const req = emitted![0][0] as CreateSftpAccountRequest;
    expect(req).toEqual({
      host: HOST_FIELDS["sftp-host"],
      port: null,
      rootPath: HOST_FIELDS["sftp-root-path"],
      username: HOST_FIELDS["sftp-username"],
      auth: "password",
      password: "hunter2",
      privateKey: null,
      passphrase: null,
    });
  });

  it("carries a custom port through as a number", async () => {
    const wrapper = mountForm();
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get("#sftp-password").setValue("hunter2");
    await wrapper.get("#sftp-port").setValue("2222");
    await wrapper.get("form").trigger("submit");

    const req = wrapper.emitted("submit")![0][0] as CreateSftpAccountRequest;
    expect(req.port).toBe(2222);
  });

  it("switches to private-key auth and requires the key, not the password", async () => {
    const wrapper = mountForm();
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get('[data-testid="sftp-auth-private-key"]').setValue(true);

    // No password field required in this mode - it must not even gate submit.
    const button = wrapper.get('[data-testid="sftp-connect"]');
    expect((button.element as HTMLButtonElement).disabled).toBe(true);

    await wrapper
      .get("#sftp-private-key")
      .setValue("-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----");
    expect((button.element as HTMLButtonElement).disabled).toBe(false);
  });

  it("emits a private-key request with the passphrase, and null password", async () => {
    const wrapper = mountForm();
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get('[data-testid="sftp-auth-private-key"]').setValue(true);
    const pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----";
    await wrapper.get("#sftp-private-key").setValue(pem);
    await wrapper.get("#sftp-passphrase").setValue("s3cr3t");
    await wrapper.get("form").trigger("submit");

    const req = wrapper.emitted("submit")![0][0] as CreateSftpAccountRequest;
    expect(req).toEqual({
      host: HOST_FIELDS["sftp-host"],
      port: null,
      rootPath: HOST_FIELDS["sftp-root-path"],
      username: HOST_FIELDS["sftp-username"],
      auth: "privateKey",
      password: null,
      privateKey: pem,
      passphrase: "s3cr3t",
    });
  });

  it("normalizes a blank passphrase to null rather than an empty string", async () => {
    const wrapper = mountForm();
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get('[data-testid="sftp-auth-private-key"]').setValue(true);
    await wrapper.get("#sftp-private-key").setValue("a-key");
    await wrapper.get("form").trigger("submit");

    const req = wrapper.emitted("submit")![0][0] as CreateSftpAccountRequest;
    expect(req.passphrase).toBeNull();
  });

  it("switching back to password auth drops a previously entered key and passphrase from the payload", async () => {
    const wrapper = mountForm();
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get('[data-testid="sftp-auth-private-key"]').setValue(true);
    await wrapper.get("#sftp-private-key").setValue("a-key");
    await wrapper.get("#sftp-passphrase").setValue("pass");
    await wrapper.get('[data-testid="sftp-auth-password"]').setValue(true);
    await wrapper.get("#sftp-password").setValue("hunter2");
    await wrapper.get("form").trigger("submit");

    const req = wrapper.emitted("submit")![0][0] as CreateSftpAccountRequest;
    expect(req.auth).toBe("password");
    expect(req.privateKey).toBeNull();
    expect(req.passphrase).toBeNull();
    expect(req.password).toBe("hunter2");
  });

  it("masks both the password and passphrase fields", async () => {
    const wrapper = mountForm();
    const password = wrapper.get('[data-testid="sftp-password"]');
    expect(password.attributes("type")).toBe("password");
    expect(password.attributes("autocomplete")).toBe("off");

    await wrapper.get('[data-testid="sftp-auth-private-key"]').setValue(true);
    const passphrase = wrapper.get('[data-testid="sftp-passphrase"]');
    expect(passphrase.attributes("type")).toBe("password");
  });

  it("does not submit while busy, and shows the parent's error", async () => {
    const wrapper = mountForm({ busy: true, errorMessage: "That path does not exist." });
    await fill(wrapper, HOST_FIELDS);
    await wrapper.get("#sftp-password").setValue("hunter2");
    await wrapper.get("form").trigger("submit");
    expect(wrapper.emitted("submit")).toBeUndefined();
    expect(wrapper.text()).toContain("That path does not exist.");
  });
});
