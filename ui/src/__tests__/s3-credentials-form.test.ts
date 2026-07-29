// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// The S3 destination's credentials step. Two things matter here beyond "does it
// render": the request it emits must match what `create_s3_account` expects
// (optional fields normalized to null rather than empty strings), and the
// secret access key must be a password field that is never echoed back - it
// exists in the webview only long enough to reach the OS keychain.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { i18n } from "../i18n";
import S3CredentialsForm from "../components/S3CredentialsForm.vue";
import { useSetupStore } from "../stores/setup";
import type { CreateS3AccountRequest } from "../ipc/types";

function mountForm(props: Record<string, unknown> = {}) {
  return mount(S3CredentialsForm, {
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

const COMPLETE = {
  "s3-endpoint": "https://example.r2.cloudflarestorage.com",
  "s3-bucket": "my-backups",
  "s3-access-key": "AKIAEXAMPLE",
  "s3-secret-key": "super-secret",
};

describe("S3CredentialsForm", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setActivePinia(createPinia());
  });

  it("keeps submit disabled until every required field is filled", async () => {
    const wrapper = mountForm();
    const button = wrapper.get('[data-testid="s3-connect"]');
    expect((button.element as HTMLButtonElement).disabled).toBe(true);

    // Endpoint + bucket alone are not enough: without a key pair the backend
    // could never authenticate.
    await fill(wrapper, {
      "s3-endpoint": COMPLETE["s3-endpoint"],
      "s3-bucket": COMPLETE["s3-bucket"],
    });
    expect((button.element as HTMLButtonElement).disabled).toBe(true);

    await fill(wrapper, {
      "s3-access-key": COMPLETE["s3-access-key"],
      "s3-secret-key": COMPLETE["s3-secret-key"],
    });
    expect((button.element as HTMLButtonElement).disabled).toBe(false);
  });

  it("emits a request with blank optional fields normalized to null", async () => {
    const wrapper = mountForm();
    await fill(wrapper, COMPLETE);
    await wrapper.get("form").trigger("submit");

    const emitted = wrapper.emitted("submit");
    expect(emitted).toHaveLength(1);
    const req = emitted![0][0] as CreateS3AccountRequest;
    expect(req).toEqual({
      endpoint: COMPLETE["s3-endpoint"],
      bucket: COMPLETE["s3-bucket"],
      // Blank optional inputs must be null, not "": the backend treats an empty
      // region as "use the default" only via null/blank normalization, and an
      // empty prefix must not become a literal "" key prefix.
      region: null,
      prefix: null,
      pathStyle: true,
      accessKeyId: COMPLETE["s3-access-key"],
      secretAccessKey: COMPLETE["s3-secret-key"],
    });
  });

  it("carries the optional region, prefix and addressing style through", async () => {
    const wrapper = mountForm();
    await fill(wrapper, { ...COMPLETE, "s3-region": " us-west-2 ", "s3-prefix": " driven/ " });
    await wrapper.get('[data-testid="s3-path-style"]').setValue(false);
    await wrapper.get("form").trigger("submit");

    const req = wrapper.emitted("submit")![0][0] as CreateS3AccountRequest;
    expect(req.region).toBe("us-west-2");
    expect(req.prefix).toBe("driven/");
    expect(req.pathStyle).toBe(false);
  });

  it("masks the secret access key", () => {
    const wrapper = mountForm();
    const secret = wrapper.get('[data-testid="s3-secret-key"]');
    expect(secret.attributes("type")).toBe("password");
    expect(secret.attributes("autocomplete")).toBe("off");
  });

  it("does not submit while busy, and shows the parent's error", async () => {
    const wrapper = mountForm({ busy: true, errorMessage: "Those credentials were rejected." });
    await fill(wrapper, COMPLETE);
    await wrapper.get("form").trigger("submit");
    expect(wrapper.emitted("submit")).toBeUndefined();
    expect(wrapper.text()).toContain("Those credentials were rejected.");
  });
});

describe("setup store S3 account creation", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setActivePinia(createPinia());
  });

  const REQ: CreateS3AccountRequest = {
    endpoint: "https://example.r2.cloudflarestorage.com",
    bucket: "my-backups",
    region: null,
    prefix: null,
    pathStyle: true,
    accessKeyId: "AKIAEXAMPLE",
    secretAccessKey: "super-secret",
  };

  it("records the account and resolves sign-in on success", async () => {
    invokeMock.mockResolvedValueOnce({
      id: "acct-1",
      email: "my-backups/",
      displayName: null,
      state: "ok",
      encryptionEnabled: false,
      createdAt: 0,
      lastSyncedAt: null,
      backendKind: "s3",
    });
    const setup = useSetupStore();
    await expect(setup.createS3Account(REQ)).resolves.toBe(true);

    expect(invokeMock).toHaveBeenCalledWith("create_s3_account", { req: REQ });
    expect(setup.accountId).toBe("acct-1");
    // There is no consent round trip, so the wizard's signed-in gate must be
    // satisfied by the create itself or the source step stays unreachable.
    expect(setup.signedIn).toBe(true);
    expect(setup.busy).toBe(false);
  });

  it("surfaces a rejection as an error code and does not advance", async () => {
    invokeMock.mockRejectedValueOnce({ code: "auth.invalid_grant", message: "nope" });
    const setup = useSetupStore();
    await expect(setup.createS3Account(REQ)).resolves.toBe(false);

    expect(setup.errorCode).toBe("auth.invalid_grant");
    expect(setup.accountId).toBeNull();
    expect(setup.signedIn).toBe(false);
    expect(setup.busy).toBe(false);
  });
});
