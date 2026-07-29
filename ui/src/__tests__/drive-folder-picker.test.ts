// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";

// Issue #7: the DriveFolderPicker must surface Google Shared Drive roots beside
// My Drive, badge them, carry the driveId back into `pickDriveFolder` when the
// user descends a Shared Drive, and publish the driveId through its `drive-id`
// v-model so the wizard persists it with the source. These tests mount the
// component directly and mock the single `invoke("pick_drive_folder")` seam.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { i18n } from "../i18n";
import DriveFolderPicker from "../components/DriveFolderPicker.vue";
import type { DriveFolderListing } from "../ipc/types";

const ACCOUNT = "acct-1";

/** The My Drive root listing: one Shared Drive root + one ordinary folder. */
const ROOT_LISTING: DriveFolderListing = {
  currentFolderId: "root",
  driveId: null,
  currentFolderPath: "",
  folders: [
    { id: "0ATeamA", name: "Team A", driveId: "0ATeamA", isSharedDrive: true },
    { id: "f-mydrive", name: "My Folder", driveId: null, isSharedDrive: false },
  ],
};

/** Inside the Shared Drive "Team A": one child folder scoped to the drive. */
const SHARED_LISTING: DriveFolderListing = {
  currentFolderId: "0ATeamA",
  driveId: "0ATeamA",
  currentFolderPath: "",
  folders: [{ id: "sub-1", name: "Sub", driveId: "0ATeamA", isSharedDrive: false }],
};

function mountPicker(backendKind?: string) {
  return mount(DriveFolderPicker, {
    props: { accountId: ACCOUNT, backendKind },
    global: { plugins: [i18n] },
  });
}

describe("DriveFolderPicker Shared Drive support (issue #7)", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    // Route each pick_drive_folder call by the driveId arg it was given.
    invokeMock.mockImplementation((cmd: string, args: Record<string, unknown>) => {
      if (cmd !== "pick_drive_folder") throw new Error(`unexpected command ${cmd}`);
      return Promise.resolve(args.driveId === "0ATeamA" ? SHARED_LISTING : ROOT_LISTING);
    });
  });

  it("loads My Drive root with a null driveId and badges the Shared Drive root", async () => {
    const wrapper = mountPicker();
    await flushPromises();

    // The root load passes a null driveId (My Drive scope).
    expect(invokeMock).toHaveBeenCalledWith("pick_drive_folder", {
      accountId: ACCOUNT,
      startFolderId: null,
      driveId: null,
    });

    // The Shared Drive root is badged; the ordinary folder is not.
    const badge = i18n.global.t("drivePicker.sharedDriveBadge");
    const items = wrapper.findAll("li");
    expect(items).toHaveLength(2);
    expect(items[0].text()).toContain(badge);
    expect(items[0].text()).toContain("Team A");
    expect(items[1].text()).not.toContain(badge);

    // The root is published as the selectable destination (null default ->
    // concrete "root"). The driveId stays null (My Drive) so it does not
    // re-emit from its null default - the null-vs-Shared switch is covered by
    // the descent test below.
    const folderIdEvents = wrapper.emitted("update:folderId");
    expect(folderIdEvents?.at(-1)?.[0]).toBe("root");
  });

  it("descends into a Shared Drive, scoping the next list to its driveId", async () => {
    const wrapper = mountPicker();
    await flushPromises();

    // Click the Shared Drive root (the first list button).
    await wrapper.findAll("li button")[0].trigger("click");
    await flushPromises();

    // The descent re-lists with the Shared Drive's driveId (corpora=drive scope).
    expect(invokeMock).toHaveBeenLastCalledWith("pick_drive_folder", {
      accountId: ACCOUNT,
      startFolderId: "0ATeamA",
      driveId: "0ATeamA",
    });

    // The published driveId + folderId now reflect the Shared Drive.
    const driveIdEvents = wrapper.emitted("update:driveId");
    expect(driveIdEvents?.at(-1)?.[0]).toBe("0ATeamA");
    const folderIdEvents = wrapper.emitted("update:folderId");
    expect(folderIdEvents?.at(-1)?.[0]).toBe("0ATeamA");

    // Only the drive's child folder is shown now.
    const items = wrapper.findAll("li");
    expect(items).toHaveLength(1);
    expect(items[0].text()).toContain("Sub");
  });
});

describe("DriveFolderPicker root label is per-destination", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue({
      currentFolderId: "root",
      driveId: null,
      currentFolderPath: "",
      folders: [],
    });
  });

  /** The root breadcrumb button plus the "Backing up to" line, which both name
   * the destination root. */
  function rootLabels(wrapper: ReturnType<typeof mountPicker>): string[] {
    return [
      wrapper.findAll("nav button")[0].text(),
      wrapper.get('[data-testid="drive-destination"]').text(),
    ];
  }

  it("names Drive's root My Drive", async () => {
    const wrapper = mountPicker("google_drive");
    await flushPromises();
    for (const label of rootLabels(wrapper)) {
      expect(label).toContain(i18n.global.t("drivePicker.root.google_drive"));
    }
  });

  it("names an S3 destination's root the bucket root, not My Drive", async () => {
    // The picker is shared by every browsable destination, so a hard-coded
    // "My Drive" was simply wrong for an S3 account sitting on its bucket root -
    // which is exactly what the maintainer saw in the running app.
    const wrapper = mountPicker("s3");
    await flushPromises();
    for (const label of rootLabels(wrapper)) {
      expect(label).toContain(i18n.global.t("drivePicker.root.s3"));
      expect(label).not.toContain("My Drive");
    }
  });

  it("falls back to a neutral root label for an unseeded backend", async () => {
    // `BackendKind::ALL` is Rust-owned and can gain a destination before the
    // locale file does; that must render neutrally, never as a Drive-ism and
    // never as a raw key.
    const wrapper = mountPicker("some_future_backend");
    await flushPromises();
    for (const label of rootLabels(wrapper)) {
      expect(label).toContain(i18n.global.t("drivePicker.rootName"));
      expect(label).not.toContain("My Drive");
      expect(label).not.toContain("drivePicker");
    }
  });

  it("falls back to the neutral label when no backend kind is given at all", async () => {
    const wrapper = mountPicker();
    await flushPromises();
    expect(wrapper.findAll("nav button")[0].text()).toContain(
      i18n.global.t("drivePicker.rootName")
    );
  });
});
