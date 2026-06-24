import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Restore store tests (SPEC s11.5; DESIGN s8.4). The seams are
// `@tauri-apps/api/core`'s `invoke` (every typed IPC wrapper routes through it)
// and `@tauri-apps/api/event`'s `listen` (the restore:progress subscription).
// Mocking both lets us drive the whole browse -> search -> select -> restore flow
// against a fake backend and fire `restore:progress` ticks on demand, asserting:
// the tree loads lazily per folder, search routes through search_files, selection
// toggles + builds RestoreItems, startRestore consumes the dialog token, and the
// progress stream accumulates to a terminal done state (with encrypted display
// names already plaintext via the fake).

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

// Capture the restore:progress handler so the test can fire events on demand.
let progressHandler: ((payload: unknown) => void) | null = null;
const unlistenMock = vi.fn();
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(
    async (event: string, cb: (e: { payload: unknown }) => void) => {
      if (event === "restore:progress") {
        progressHandler = (payload: unknown) => cb({ payload });
        return unlistenMock;
      }
      return vi.fn();
    },
  ),
}));

import { useRestoreStore } from "../stores/restore";
import type {
  FileSearchHitDto,
  RemoteEntryDto,
  RestoreJobStatus,
  SourceDto,
} from "../ipc/types";

function source(id: string, name: string): SourceDto {
  return {
    id,
    accountId: "acct-1",
    displayName: name,
    enabled: true,
    localPath: "/home/u/" + name,
    driveFolderId: "drive-" + id,
    driveFolderPath: name,
    encryptionEnabled: true,
    respectGitignore: true,
    includePatterns: [],
    excludePatterns: [],
    deepVerifyIntervalSecs: 604800,
    lastFullScanAt: null,
    createdAt: 0,
  };
}

function folder(name: string, prefix = ""): RemoteEntryDto {
  return {
    relativePath: prefix ? `${prefix}/${name}` : name,
    name,
    isDir: true,
    size: 0,
    status: null,
    restorable: false,
  };
}

function file(
  name: string,
  prefix = "",
  restorable = true,
  size = 10,
): RemoteEntryDto {
  return {
    relativePath: prefix ? `${prefix}/${name}` : name,
    name,
    isDir: false,
    size,
    status: "synced",
    restorable,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  progressHandler = null;
  unlistenMock.mockReset();
});

describe("restore store", () => {
  it("loads sources and browses the root tree lazily", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve([folder("src"), file("a.txt")]);
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.loadSources();

    // Auto-selected the first source and loaded its root tree.
    expect(store.sourceId).toBe("s1");
    expect(store.prefix).toBe("");
    expect(store.rows.length).toBe(2);
    // Folders sort before files (the backend returns them ordered). The tree rows
    // are RemoteEntryDto (carry `name`); assert via the typed `nodes` list.
    expect(store.nodes[0].name).toBe("src");
    expect(store.nodes[1].name).toBe("a.txt");

    // list_remote_tree was called for the root prefix (lazy per folder).
    const treeCall = invokeMock.mock.calls.find(
      (c) => c[0] === "list_remote_tree",
    );
    expect(treeCall?.[1]).toMatchObject({ sourceId: "s1", prefix: "" });
  });

  it("descends into a folder and back via breadcrumbs", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree") {
        const prefix = (args as { prefix: string }).prefix;
        if (prefix === "") return Promise.resolve([folder("src")]);
        if (prefix === "src")
          return Promise.resolve([file("main.rs", "src")]);
        return Promise.resolve([]);
      }
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.loadSources();
    await store.openFolder("src");
    expect(store.prefix).toBe("src");
    expect(store.breadcrumbs).toEqual(["src"]);
    expect(store.nodes[0].name).toBe("main.rs");

    // Navigate back to root via the breadcrumb.
    await store.goToBreadcrumb(-1);
    expect(store.prefix).toBe("");
    expect(store.nodes[0].name).toBe("src");
  });

  it("routes search through search_files and shows hits", async () => {
    const hits: FileSearchHitDto[] = [
      {
        sourceId: "s1",
        relativePath: "src/main.rs",
        status: "synced",
        restorable: true,
      },
    ];
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree") return Promise.resolve([]);
      if (cmd === "search_files") return Promise.resolve(hits);
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.loadSources();
    await store.runSearch("*.rs");

    expect(store.isSearching).toBe(true);
    expect(store.rows).toEqual(hits);
    const searchCall = invokeMock.mock.calls.find(
      (c) => c[0] === "search_files",
    );
    expect(searchCall?.[1]).toMatchObject({ sourceId: "s1", query: "*.rs" });

    // Clearing the search returns to the tree.
    await store.runSearch("");
    expect(store.isSearching).toBe(false);
  });

  it("toggles selection and builds RestoreItems", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve([file("a.txt"), file("b.txt")]);
      return Promise.resolve([]);
    });
    const store = useRestoreStore();
    await store.loadSources();

    store.toggleSelect("s1", "a.txt");
    store.toggleSelect("s1", "b.txt");
    expect(store.selectedCount).toBe(2);
    expect(store.isSelected("s1", "a.txt")).toBe(true);

    // The RestoreItems decode the (sourceId, relativePath) keys correctly.
    const items = store.selectedItems();
    expect(items).toContainEqual({ sourceId: "s1", relativePath: "a.txt" });
    expect(items).toContainEqual({ sourceId: "s1", relativePath: "b.txt" });

    // Un-toggle clears it.
    store.toggleSelect("s1", "a.txt");
    expect(store.selectedCount).toBe(1);
    store.clearSelection();
    expect(store.selectedCount).toBe(0);
  });

  it("restores selected files with a dialog token and accumulates progress to done", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve([file("secret.bin")]);
      if (cmd === "restore_files") return Promise.resolve("job-1");
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.subscribeProgress();
    await store.loadSources();

    // Select a file + record a dialog-derived destination.
    store.toggleSelect("s1", "secret.bin");
    store.setDestination("/home/u/restored", "tok-abc");
    expect(store.canRestore).toBe(true);

    await store.startRestore();

    // restore_files was called with the items + the dialog token (no raw path).
    const call = invokeMock.mock.calls.find((c) => c[0] === "restore_files");
    expect(call?.[1]).toMatchObject({
      items: [{ sourceId: "s1", relativePath: "secret.bin" }],
      destToken: "tok-abc",
    });
    // The one-shot token is consumed after starting (a fresh pick is required).
    expect(store.destToken).toBeNull();
    expect(store.restoring).toBe(true);

    // Fire a mid-restore progress tick (encrypted name shown as plaintext).
    const inProgress: RestoreJobStatus = {
      jobId: "job-1",
      totalFiles: 1,
      completedFiles: 0,
      failedFiles: 0,
      totalBytes: 100,
      bytesDone: 50,
      currentFile: "secret.bin",
      done: false,
      files: [
        {
          relativePath: "secret.bin",
          state: "restoring",
          bytesDone: 50,
          bytesTotal: 100,
          errorCode: null,
        },
      ],
    };
    expect(progressHandler).not.toBeNull();
    progressHandler?.(inProgress);
    expect(store.job?.bytesDone).toBe(50);
    expect(store.job?.currentFile).toBe("secret.bin");
    expect(store.restoring).toBe(true);

    // Fire the terminal done tick.
    const done: RestoreJobStatus = {
      ...inProgress,
      completedFiles: 1,
      bytesDone: 100,
      currentFile: null,
      done: true,
      files: [
        {
          relativePath: "secret.bin",
          state: "done",
          bytesDone: 100,
          bytesTotal: 100,
          errorCode: null,
        },
      ],
    };
    progressHandler?.(done);
    expect(store.job?.done).toBe(true);
    expect(store.job?.completedFiles).toBe(1);
    // The terminal tick re-enables the controls.
    expect(store.restoring).toBe(false);
  });

  it("unsubscribes from restore:progress without leaking", async () => {
    invokeMock.mockResolvedValue([]);
    const store = useRestoreStore();
    await store.subscribeProgress();
    store.unsubscribeProgress();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
  });
});
