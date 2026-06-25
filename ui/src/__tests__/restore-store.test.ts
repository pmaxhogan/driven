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
  RemoteTreeDto,
  RestoreJobStatus,
  SourceDto,
} from "../ipc/types";

/** Wrap entries as the RemoteTreeDto the backend now returns (M8-P2-1). */
function tree(entries: RemoteEntryDto[], truncated = false): RemoteTreeDto {
  return { entries, truncated };
}

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
        return Promise.resolve(tree([folder("src"), file("a.txt")]));
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
        if (prefix === "") return Promise.resolve(tree([folder("src")]));
        if (prefix === "src")
          return Promise.resolve(tree([file("main.rs", "src")]));
        return Promise.resolve(tree([]));
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
      if (cmd === "list_remote_tree") return Promise.resolve(tree([]));
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
        return Promise.resolve(tree([file("a.txt"), file("b.txt")]));
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
    // The seeded snapshot getRestoreJob returns right after start (before any
    // live tick) - the reconcile path (M8-P2-4).
    const seeded: RestoreJobStatus = {
      jobId: "job-1",
      totalFiles: 1,
      completedFiles: 0,
      failedFiles: 0,
      totalBytes: 100,
      bytesDone: 0,
      currentFile: null,
      done: false,
      cancelled: false,
      files: [
        {
          relativePath: "secret.bin",
          state: "pending",
          bytesDone: 0,
          bytesTotal: 100,
          errorCode: null,
        },
      ],
    };
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve(tree([file("secret.bin")]));
      if (cmd === "restore_files") return Promise.resolve("job-1");
      if (cmd === "get_restore_job") return Promise.resolve(seeded);
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
      cancelled: false,
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

  it("reconciles the active job by id on (re)subscription after a remount (M8-P2-4)", async () => {
    // Simulate: a restore was started (activeJobId set), then the view remounts
    // and re-subscribes. subscribeProgress must fetch getRestoreJob(jobId) so the
    // current state is recovered even if a terminal event was missed.
    const terminal: RestoreJobStatus = {
      jobId: "job-9",
      totalFiles: 1,
      completedFiles: 1,
      failedFiles: 0,
      totalBytes: 100,
      bytesDone: 100,
      currentFile: null,
      done: true,
      cancelled: false,
      files: [
        {
          relativePath: "a.bin",
          state: "done",
          bytesDone: 100,
          bytesTotal: 100,
          errorCode: null,
        },
      ],
    };
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve(tree([file("a.bin")]));
      if (cmd === "restore_files") return Promise.resolve("job-9");
      if (cmd === "get_restore_job") return Promise.resolve(terminal);
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.loadSources();
    store.toggleSelect("s1", "a.bin");
    store.setDestination("/home/u/restored", "tok-1");
    await store.startRestore();
    expect(store.activeJobId).toBe("job-9");

    // Remount: unsubscribe then re-subscribe; the re-subscribe reconciles by id.
    store.unsubscribeProgress();
    await store.subscribeProgress();

    const getCall = invokeMock.mock.calls.find(
      (c) => c[0] === "get_restore_job",
    );
    expect(getCall?.[1]).toMatchObject({ job: "job-9" });
    // The reconciled terminal state is reflected (controls re-enabled).
    expect(store.job?.done).toBe(true);
    expect(store.restoring).toBe(false);
  });

  it("cancels the active job and reflects a terminal cancelled status (M8-P1-1)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve(tree([file("big.bin")]));
      if (cmd === "restore_files") return Promise.resolve("job-c");
      if (cmd === "cancel_restore_job") return Promise.resolve(null);
      if (cmd === "get_restore_job")
        return Promise.resolve({
          jobId: "job-c",
          totalFiles: 1,
          completedFiles: 0,
          failedFiles: 0,
          totalBytes: 100,
          bytesDone: 0,
          currentFile: "big.bin",
          done: false,
          cancelled: false,
          files: [
            {
              relativePath: "big.bin",
              state: "restoring",
              bytesDone: 0,
              bytesTotal: 100,
              errorCode: null,
            },
          ],
        } as RestoreJobStatus);
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.subscribeProgress();
    await store.loadSources();
    store.toggleSelect("s1", "big.bin");
    store.setDestination("/home/u/restored", "tok-c");
    await store.startRestore();
    expect(store.activeJobId).toBe("job-c");

    // Request cancel: the cancel IPC is invoked with the job id; cancelling gates.
    await store.cancelRestore();
    const cancelCall = invokeMock.mock.calls.find(
      (c) => c[0] === "cancel_restore_job",
    );
    expect(cancelCall?.[1]).toMatchObject({ job: "job-c" });
    expect(store.cancelling).toBe(true);

    // The backend emits a terminal CANCELLED status; the store clears the flags.
    const cancelled: RestoreJobStatus = {
      jobId: "job-c",
      totalFiles: 1,
      completedFiles: 0,
      failedFiles: 0,
      totalBytes: 100,
      bytesDone: 30,
      currentFile: null,
      done: true,
      cancelled: true,
      files: [
        {
          relativePath: "big.bin",
          state: "cancelled",
          bytesDone: 30,
          bytesTotal: 100,
          errorCode: null,
        },
      ],
    };
    progressHandler?.(cancelled);
    expect(store.job?.cancelled).toBe(true);
    expect(store.job?.done).toBe(true);
    expect(store.restoring).toBe(false);
    expect(store.cancelling).toBe(false);
  });

  it("surfaces a backend destination-collision rejection as a localizable errorCode (R3-P1-1)", async () => {
    // R3-P1-1: the backend now REJECTS a restore whose selected items collide at
    // the destination (duplicate / case-folded / file-vs-dir) with the
    // internal.invalid_input code. The store must surface that code (so the view
    // renders t(`errors.internal.invalid_input.long`)) and NOT consume the dialog
    // token (so the user can fix the selection and retry without re-picking).
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([source("s1", "Documents")]);
      if (cmd === "list_remote_tree")
        return Promise.resolve(tree([file("foo.txt"), file("Foo.txt")]));
      if (cmd === "restore_files")
        // The typed IPC wrapper normalizes a thrown command error to { code }.
        return Promise.reject({ code: "internal.invalid_input" });
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.loadSources();
    store.toggleSelect("s1", "foo.txt");
    store.toggleSelect("s1", "Foo.txt");
    store.setDestination("/home/u/restored", "tok-collide");

    await store.startRestore();

    expect(store.errorCode).toBe("internal.invalid_input");
    // The restore did not start: no active job, controls re-enabled.
    expect(store.activeJobId).toBeNull();
    expect(store.restoring).toBe(false);
  });

  it("clears the cross-source selection when the active source changes (R3-P1-1 defense in depth)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources")
        return Promise.resolve([
          source("s1", "Documents"),
          source("s2", "Photos"),
        ]);
      if (cmd === "list_remote_tree")
        return Promise.resolve(tree([file("foo.txt")]));
      return Promise.resolve([]);
    });

    const store = useRestoreStore();
    await store.loadSources();
    // Auto-selected s1; select a file there.
    expect(store.sourceId).toBe("s1");
    store.toggleSelect("s1", "foo.txt");
    expect(store.selectedCount).toBe(1);

    // Switching to a DIFFERENT source clears the accumulated selection, so two
    // sources' identically-named files cannot silently pile up into one restore.
    await store.selectSource("s2");
    expect(store.sourceId).toBe("s2");
    expect(store.selectedCount).toBe(0);

    // Re-selecting the SAME source does not clear (no source change).
    store.toggleSelect("s2", "foo.txt");
    await store.selectSource("s2");
    expect(store.selectedCount).toBe(1);
  });
});
