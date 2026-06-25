// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

// R8-P2-1: the recovery reveal/ack error handling must normalize a Tauri
// STRUCTURED error ({ code, message }) into its stable SPEC s24 code and render
// the LOCALIZED `t(\`errors.${code}.long\`)` string - never `String(e)` (which
// renders a structured error as `[object Object]`) and never the raw backend
// English `message`. These tests drive the SourceTable post-restart reveal/ack
// panel and the AddSourceWizard reveal step and assert the rendered error text is
// the localized long message for the code, not `[object Object]`.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

import { i18n } from "../i18n";
import SourceTable from "../components/SourceTable.vue";
import AddSourceWizard from "../components/AddSourceWizard.vue";
import RecoveryPhraseReveal from "../components/RecoveryPhraseReveal.vue";
import { useAccountsStore } from "../stores/accounts";

const FAKE_ACCOUNT = { id: "acct-1", email: "u@example.com" };

const PENDING_SOURCE = {
  id: "src-pending",
  accountId: "acct-1",
  displayName: "Docs",
  enabled: false,
  localPath: "/home/u/Docs",
  driveFolderId: "folder-1",
  driveFolderPath: "/Backups/Docs",
  encryptionEnabled: true,
  respectGitignore: true,
  includePatterns: [],
  excludePatterns: [],
  deepVerifyIntervalSecs: 604800,
  lastFullScanAt: null,
  createdAt: 0,
  pendingRecoveryAck: true,
};

// A structured Tauri error, exactly as `invoke` rejects: an object with a stable
// `code` (the i18n key) plus backend English `message`. `String(this)` is
// `[object Object]` - the bug R8-P2-1 fixes.
const STRUCTURED_ERROR = {
  code: "crypto.key_missing",
  message: "account master key unavailable: keychain entry not found",
};

const EXPECTED_LONG = i18n.global.t("errors.crypto.key_missing.long");

beforeEach(() => {
  invokeMock.mockReset();
  setActivePinia(createPinia());
  // Default backend responses for the SourceTable onMounted refresh.
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "list_sources":
        return Promise.resolve([PENDING_SOURCE]);
      case "list_accounts":
        return Promise.resolve([FAKE_ACCOUNT]);
      default:
        return Promise.resolve(null);
    }
  });
});

describe("SourceTable reveal/ack error localization (R8-P2-1)", () => {
  it("renders a STRUCTURED reveal error as the localized message, not [object Object]", async () => {
    const wrapper = mount(SourceTable, { global: { plugins: [i18n] } });
    await flushPromises();

    // Open the post-restart reveal/ack panel for the pending source.
    await wrapper.get('[data-testid="reveal-ack-button"]').trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="reveal-ack-panel"]').exists()).toBe(true);

    // The RecoveryPhraseReveal child surfaces a structured backend reveal error.
    const reveal = wrapper.getComponent(RecoveryPhraseReveal);
    reveal.vm.$emit("reveal-error", STRUCTURED_ERROR);
    await flushPromises();

    const text = wrapper.text();
    expect(text).toContain(EXPECTED_LONG);
    expect(text).not.toContain("[object Object]");
    // The raw backend English must not leak into the UI.
    expect(text).not.toContain("keychain entry not found");
  });

  it("renders a STRUCTURED ack error as the localized message, not [object Object]", async () => {
    // The ack IPC rejects with the structured error; reveal succeeds.
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case "list_sources":
          return Promise.resolve([PENDING_SOURCE]);
        case "list_accounts":
          return Promise.resolve([FAKE_ACCOUNT]);
        case "reveal_recovery_phrase":
          return Promise.resolve(["alpha", "bravo", "charlie"]);
        case "ack_recovery_phrase_saved":
          return Promise.reject(STRUCTURED_ERROR);
        default:
          return Promise.resolve(null);
      }
    });

    const wrapper = mount(SourceTable, { global: { plugins: [i18n] } });
    await flushPromises();

    await wrapper.get('[data-testid="reveal-ack-button"]').trigger("click");
    await flushPromises();

    // Drive the gate (reveal shown + acknowledged) so confirm is reachable, then
    // confirm - the ack rejects and the error must localize.
    const reveal = wrapper.getComponent(RecoveryPhraseReveal);
    reveal.vm.$emit("update:revealed", true);
    reveal.vm.$emit("update:confirmed", true);
    await flushPromises();

    await wrapper.get('[data-testid="reveal-ack-confirm"]').trigger("click");
    await flushPromises();

    const text = wrapper.text();
    expect(text).toContain(EXPECTED_LONG);
    expect(text).not.toContain("[object Object]");
    expect(text).not.toContain("keychain entry not found");
  });
});

describe("AddSourceWizard reveal-step error localization (R8-P2-1)", () => {
  it("renders a STRUCTURED reveal error as the localized message, not [object Object]", async () => {
    const accounts = useAccountsStore();
    accounts.accounts = [{ id: "acct-1", email: "u@example.com" }] as never;

    const wrapper = mount(AddSourceWizard, { global: { plugins: [i18n] } });
    // Drive the wizard straight to the reveal step with a pending-ack created
    // source (the post-encrypted-add state) via its internal refs.
    const vm = wrapper.vm as unknown as {
      open: boolean;
      revealing: boolean;
      createdSource: { id: string } | null;
      pendingRecoveryAck: boolean;
      recoveryPhrase: string[];
    };
    vm.open = true;
    vm.createdSource = { id: "src-1" } as never;
    vm.pendingRecoveryAck = true;
    vm.recoveryPhrase = ["alpha", "bravo", "charlie"];
    vm.revealing = true;
    await flushPromises();

    const reveal = wrapper.getComponent(RecoveryPhraseReveal);
    reveal.vm.$emit("reveal-error", STRUCTURED_ERROR);
    await flushPromises();

    const err = wrapper.find('[data-testid="reveal-error"]');
    expect(err.exists()).toBe(true);
    expect(err.text()).toBe(EXPECTED_LONG);
    expect(err.text()).not.toContain("[object Object]");
    expect(wrapper.text()).not.toContain("keychain entry not found");
  });
});
