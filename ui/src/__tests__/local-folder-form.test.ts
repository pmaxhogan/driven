// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// The local / removable-folder destination's setup step. Three things matter
// here beyond "does it render":
//
//  1. the folder is chosen through the BACKEND-owned native dialog, never typed
//     (SPEC s11.6.1 / C1) - so there is no free-text path field to test, and a
//     cancelled dialog must leave the form un-submittable rather than erroring;
//  2. the request it emits matches what `create_local_folder_account` expects
//     (an optional display name normalized to null rather than an empty string);
//  3. the two warnings that matter for this destination - no trash, and what
//     happens when the drive is unplugged - are actually on screen. They are the
//     difference between an informed choice and a nasty surprise.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { i18n } from "../i18n";
import LocalFolderForm from "../components/LocalFolderForm.vue";
import type { CreateLocalFolderAccountRequest } from "../ipc/types";

function mountForm(props: Record<string, unknown> = {}) {
  const submitted: CreateLocalFolderAccountRequest[] = [];
  const wrapper = mount(LocalFolderForm, {
    props: {
      ...props,
      onSubmit: (req: CreateLocalFolderAccountRequest) => submitted.push(req),
    },
    global: { plugins: [i18n] },
  });
  return { wrapper, submitted };
}

describe("LocalFolderForm", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setActivePinia(createPinia());
  });

  it("renders the folder chooser and disables submit until a folder is chosen", async () => {
    const { wrapper } = mountForm();
    expect(wrapper.find('[data-testid="local-folder-form"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="local-folder-choose"]').exists()).toBe(true);
    // There is deliberately no free-text path input: the path must come from the
    // backend-owned dialog so the backend can trust it.
    expect(wrapper.find('[data-testid="local-folder-path"]').exists()).toBe(false);
    const submit = wrapper.get('[data-testid="local-folder-connect"]');
    expect((submit.element as HTMLButtonElement).disabled).toBe(true);
  });

  it("shows the chosen folder and emits the create request", async () => {
    invokeMock.mockResolvedValueOnce({ path: "/Volumes/Backup", token: "tok-1" });
    const { wrapper, submitted } = mountForm();

    await wrapper.get('[data-testid="local-folder-choose"]').trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("pick_folder_dialog", undefined);
    expect(wrapper.get('[data-testid="local-folder-path"]').text()).toBe("/Volumes/Backup");
    // The label defaults to the folder's own name.
    expect(
      (wrapper.get('[data-testid="local-folder-name"]').element as HTMLInputElement).value
    ).toBe("Backup");

    const submit = wrapper.get('[data-testid="local-folder-connect"]');
    expect((submit.element as HTMLButtonElement).disabled).toBe(false);
    await wrapper.get('[data-testid="local-folder-form"]').trigger("submit");

    expect(submitted).toEqual([{ root: "/Volumes/Backup", displayName: "Backup" }]);
  });

  it("normalizes a blank display name to null rather than an empty string", async () => {
    invokeMock.mockResolvedValueOnce({ path: "/Volumes/Backup", token: "tok-1" });
    const { wrapper, submitted } = mountForm();
    await wrapper.get('[data-testid="local-folder-choose"]').trigger("click");
    await flushPromises();

    await wrapper.get('[data-testid="local-folder-name"]').setValue("   ");
    await wrapper.get('[data-testid="local-folder-form"]').trigger("submit");

    expect(submitted).toEqual([{ root: "/Volumes/Backup", displayName: null }]);
  });

  it("treats a cancelled dialog as no choice, not an error", async () => {
    invokeMock.mockRejectedValueOnce(new Error("cancelled"));
    const { wrapper, submitted } = mountForm();

    await wrapper.get('[data-testid="local-folder-choose"]').trigger("click");
    await flushPromises();

    expect(wrapper.find('[data-testid="local-folder-path"]').exists()).toBe(false);
    expect(
      (wrapper.get('[data-testid="local-folder-connect"]').element as HTMLButtonElement).disabled
    ).toBe(true);
    await wrapper.get('[data-testid="local-folder-form"]').trigger("submit");
    expect(submitted).toEqual([]);
  });

  it("warns that this destination has no trash and what an unplugged drive does", () => {
    const { wrapper } = mountForm();
    const text = wrapper.text();
    expect(wrapper.find('[data-testid="local-folder-trash-warning"]').exists()).toBe(true);
    expect(text).toContain("deleted permanently");
    expect(text).toContain("reconnect");
  });

  it("surfaces the parent's error message and blocks submit while busy", async () => {
    invokeMock.mockResolvedValueOnce({ path: "/Volumes/Backup", token: "tok-1" });
    const { wrapper } = mountForm({ errorMessage: "That folder is read-only." });
    await wrapper.get('[data-testid="local-folder-choose"]').trigger("click");
    await flushPromises();

    const alert = wrapper.get('[role="alert"]');
    expect(alert.text()).toBe("That folder is read-only.");

    await wrapper.setProps({ busy: true });
    expect(
      (wrapper.get('[data-testid="local-folder-connect"]').element as HTMLButtonElement).disabled
    ).toBe(true);
  });
});
