import { describe, it, expect, vi, beforeEach } from "vitest";

// Streaming exclusion-preview controller tests (SPEC s11.2; DESIGN s8.5 step 3).
// The seams are `@tauri-apps/api/core`'s `invoke` (the start / cancel commands)
// and `@tauri-apps/api/event`'s `listen` (the batch / done stream). Mocking both
// lets us drive the controller against a fake backend and fire batches by hand,
// asserting: batches fold into a tree with parents before children, a superseded
// generation's events are discarded, the totals track the stream (and stay exact
// past a truncation), and the "+"/"-" globs match the Rust matcher's form.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

let batchHandler: ((payload: unknown) => void) | null = null;
let doneHandler: ((payload: unknown) => void) | null = null;
const unlistenBatch = vi.fn();
const unlistenDone = vi.fn();
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
    if (event === "exclusion_preview:batch") {
      batchHandler = (payload: unknown) => cb({ payload });
      return unlistenBatch;
    }
    if (event === "exclusion_preview:done") {
      doneHandler = (payload: unknown) => cb({ payload });
      return unlistenDone;
    }
    return vi.fn();
  }),
}));

import {
  anchoredPatternForPath,
  appendPatternLine,
  createExclusionPreview,
  type ExclusionPreviewController,
} from "../stores/exclusionPreview";
import type { ExclusionPreviewBatch, ExclusionPreviewNode } from "../ipc/types";

function node(path: string, isDir: boolean, included: boolean, size = 0): ExclusionPreviewNode {
  return { path, isDir, included, size };
}

function batch(
  previewId: string,
  nodes: ExclusionPreviewNode[],
  over: Partial<ExclusionPreviewBatch> = {}
): ExclusionPreviewBatch {
  const files = nodes.filter((n) => !n.isDir);
  return {
    previewId,
    nodes,
    includedCount: files.filter((n) => n.included).length,
    excludedCount: files.filter((n) => !n.included).length,
    includedBytes: files.filter((n) => n.included).reduce((a, n) => a + n.size, 0),
    truncated: false,
    ...over,
  };
}

/** Start a preview whose generation id is `id`, and flush the first frame. */
async function started(id = "gen-1"): Promise<ExclusionPreviewController> {
  const preview = createExclusionPreview();
  await preview.subscribe();
  invokeMock.mockResolvedValue(id);
  await preview.start({
    sourceId: "src-1",
    respectGitignore: true,
    includePatterns: [],
    excludePatterns: [],
  });
  return preview;
}

beforeEach(() => {
  invokeMock.mockReset();
  batchHandler = null;
  doneHandler = null;
  unlistenBatch.mockReset();
  unlistenDone.mockReset();
});

describe("anchoredPatternForPath", () => {
  // THE SAME TABLE the Rust `anchored_pattern_vectors_are_stable` test asserts
  // against `driven_core::exclude::anchored_pattern_for_path` (which is in turn
  // verified against the real `build_source_matcher`). The two implementations
  // must agree character for character - a click appends a rule the RUST matcher
  // then interprets. Change both sides together.
  const vectors: Array<[string, boolean, string | null]> = [
    ["notes.txt", false, "/notes.txt"],
    ["docs/notes.txt", false, "/docs/notes.txt"],
    ["docs", true, "/docs/"],
    ["a/b/c", true, "/a/b/c/"],
    ["odd[1].txt", false, "/odd\\[1\\].txt"],
    ["alt{a,b}.txt", false, "/alt\\{a,b\\}.txt"],
    ["star*.txt", false, "/star\\*.txt"],
    ["q?.txt", false, "/q\\?.txt"],
    ["back\\slash.txt", false, "/back\\\\slash.txt"],
    ["!bang.txt", false, "/!bang.txt"],
    ["#hash.txt", false, "/#hash.txt"],
    ["My Documents/a.txt", false, "/My Documents/a.txt"],
    ["trailing .txt", false, "/trailing .txt"],
    ["trails ", false, "/trails\\ "],
    ["", false, null],
    ["two\nlines.txt", false, null],
    ["carriage\rreturn.txt", false, null],
    ["tabbed\t", false, null],
  ];

  it.each(vectors)("maps %j (isDir=%s) to %j", (rel, isDir, expected) => {
    expect(anchoredPatternForPath(rel, isDir)).toBe(expected);
  });
});

describe("appendPatternLine", () => {
  it("appends as a new line and never duplicates an existing rule", () => {
    expect(appendPatternLine("", "/a.txt")).toBe("/a.txt");
    expect(appendPatternLine("*.log", "/a.txt")).toBe("*.log\n/a.txt");
    // A textarea already ending in a newline must not gain a blank line.
    expect(appendPatternLine("*.log\n", "/a.txt")).toBe("*.log\n/a.txt");
    // Clicking the same row twice must not stack duplicate rules (they count
    // against the source's 256-pattern cap).
    expect(appendPatternLine("*.log\n/a.txt", "/a.txt")).toBe("*.log\n/a.txt");
    // The editors split on commas too, so a comma-listed duplicate also counts.
    expect(appendPatternLine("*.log,/a.txt", "/a.txt")).toBe("*.log,/a.txt");
  });
});

describe("createExclusionPreview", () => {
  it("starts a streaming preview and reports it as scanning", async () => {
    const preview = await started();
    expect(invokeMock).toHaveBeenCalledWith("preview_exclusions_start", {
      req: {
        sourceId: "src-1",
        localPathToken: undefined,
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
      },
    });
    expect(preview.currentPreviewId()).toBe("gen-1");
    expect(preview.scanning.value).toBe(true);
    expect(preview.complete.value).toBe(false);
  });

  it("folds streamed batches into a tree, coalescing them into one flush", async () => {
    const preview = await started();
    const before = preview.treeVersion.value;

    batchHandler!(batch("gen-1", [node("docs", true, true), node("keep.txt", false, true, 5)]));
    batchHandler!(
      batch("gen-1", [node("docs/a.txt", false, true, 3), node("docs/b.log", false, false, 1)])
    );
    // Buffered, not yet applied: the whole burst costs ONE reactive update.
    expect(preview.roots.value).toHaveLength(0);
    preview.flush();

    expect(preview.treeVersion.value).toBe(before + 1);
    expect(preview.roots.value.map((n) => n.path)).toEqual(["docs", "keep.txt"]);
    const docs = preview.nodeAt("docs");
    expect(docs?.children.map((n) => n.path)).toEqual(["docs/a.txt", "docs/b.log"]);
    expect(docs?.depth).toBe(0);
    expect(preview.nodeAt("docs/a.txt")?.depth).toBe(1);
    expect(preview.nodeAt("docs/b.log")?.included).toBe(false);
    expect(preview.nodeAt("keep.txt")?.name).toBe("keep.txt");
  });

  it("sorts each container directories-first then by name", async () => {
    const preview = await started();
    batchHandler!(
      batch("gen-1", [
        node("zebra.txt", false, true),
        node("alpha", true, true),
        node("apple.txt", false, true),
        node("beta", true, true),
      ])
    );
    preview.flush();
    expect(preview.roots.value.map((n) => n.name)).toEqual([
      "alpha",
      "beta",
      "apple.txt",
      "zebra.txt",
    ]);
  });

  it("tracks the running totals from the batches", async () => {
    const preview = await started();
    batchHandler!(batch("gen-1", [node("a.txt", false, true, 100), node("b.log", false, false)]));
    preview.flush();
    expect(preview.includedCount.value).toBe(1);
    expect(preview.excludedCount.value).toBe(1);
    expect(preview.includedBytes.value).toBe(100);
    expect(preview.truncated.value).toBe(false);
  });

  it("keeps the totals exact after the streamed tree truncates", async () => {
    const preview = await started();
    // The backend caps the NODE stream but keeps counting: a truncated batch
    // carries no (or partial) nodes while its counts race far ahead.
    batchHandler!(
      batch("gen-1", [node("a.txt", false, true, 10)], {
        includedCount: 9_000,
        excludedCount: 1_200,
        includedBytes: 987_654,
        truncated: true,
      })
    );
    preview.flush();
    expect(preview.truncated.value).toBe(true);
    expect(preview.includedCount.value).toBe(9_000);
    expect(preview.excludedCount.value).toBe(1_200);
    expect(preview.includedBytes.value).toBe(987_654);
    expect(preview.roots.value).toHaveLength(1);
  });

  it("discards batches from a superseded generation", async () => {
    const preview = await started();
    batchHandler!(batch("gen-1", [node("mine.txt", false, true)]));
    // A walk the backend already cancelled can still have events in flight.
    batchHandler!(batch("gen-STALE", [node("stale.txt", false, true)]));
    preview.flush();

    expect(preview.roots.value.map((n) => n.path)).toEqual(["mine.txt"]);
    expect(preview.nodeAt("stale.txt")).toBeUndefined();
  });

  it("ignores a done event from a superseded generation", async () => {
    const preview = await started();
    doneHandler!({
      previewId: "gen-STALE",
      includedCount: 999,
      excludedCount: 999,
      includedBytes: 999,
      truncated: false,
      cancelled: false,
    });
    preview.flush();
    expect(preview.scanning.value).toBe(true);
    expect(preview.complete.value).toBe(false);
    expect(preview.includedCount.value).toBe(0);
  });

  it("replays events that arrive before the generation id resolves", async () => {
    // The walk starts before `preview_exclusions_start` returns, so its first
    // batches can legitimately beat the id back to the webview.
    const preview = createExclusionPreview();
    await preview.subscribe();
    let resolveStart: (id: string) => void = () => undefined;
    invokeMock.mockImplementation(
      () =>
        new Promise<string>((res) => {
          resolveStart = res;
        })
    );
    const starting = preview.start({
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    });

    batchHandler!(batch("gen-1", [node("early.txt", false, true, 7)]));
    batchHandler!(batch("gen-OTHER", [node("not-mine.txt", false, true)]));
    resolveStart("gen-1");
    await starting;
    preview.flush();

    expect(preview.roots.value.map((n) => n.path)).toEqual(["early.txt"]);
    expect(preview.nodeAt("not-mine.txt")).toBeUndefined();
    expect(preview.includedBytes.value).toBe(7);
  });

  it("marks the scan complete on a done event, with the final totals", async () => {
    const preview = await started();
    doneHandler!({
      previewId: "gen-1",
      includedCount: 12,
      excludedCount: 4,
      includedBytes: 4096,
      truncated: false,
      cancelled: false,
    });
    preview.flush();
    expect(preview.scanning.value).toBe(false);
    expect(preview.complete.value).toBe(true);
    expect(preview.includedCount.value).toBe(12);
    expect(preview.excludedCount.value).toBe(4);
    expect(preview.includedBytes.value).toBe(4096);
  });

  it("does not claim completion for a CANCELLED walk", async () => {
    const preview = await started();
    doneHandler!({
      previewId: "gen-1",
      includedCount: 3,
      excludedCount: 0,
      includedBytes: 1,
      truncated: false,
      cancelled: true,
    });
    preview.flush();
    expect(preview.scanning.value).toBe(false);
    expect(preview.complete.value).toBe(false);
  });

  it("clears the tree when a new preview starts", async () => {
    const preview = await started();
    batchHandler!(batch("gen-1", [node("old.txt", false, true, 9)]));
    preview.flush();
    expect(preview.roots.value).toHaveLength(1);

    invokeMock.mockResolvedValue("gen-2");
    await preview.start({
      sourceId: "src-1",
      respectGitignore: false,
      includePatterns: [],
      excludePatterns: ["*.txt"],
    });
    expect(preview.roots.value).toHaveLength(0);
    expect(preview.includedCount.value).toBe(0);
    expect(preview.includedBytes.value).toBe(0);
    expect(preview.currentPreviewId()).toBe("gen-2");

    // And the OLD generation's still-in-flight events cannot come back.
    batchHandler!(batch("gen-1", [node("old.txt", false, true, 9)]));
    preview.flush();
    expect(preview.roots.value).toHaveLength(0);
  });

  it("abandons and cancels a start that a newer one overtook", async () => {
    const preview = createExclusionPreview();
    await preview.subscribe();
    const resolvers: Array<(id: string) => void> = [];
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "preview_exclusions_start") {
        return new Promise<string>((res) => resolvers.push(res));
      }
      return Promise.resolve(undefined);
    });
    const req = {
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    };
    const first = preview.start(req);
    const second = preview.start(req);
    // The SECOND start resolves first; the first must not then claim the tree.
    resolvers[1]("gen-2");
    await second;
    resolvers[0]("gen-1");
    await first;

    expect(preview.currentPreviewId()).toBe("gen-2");
    expect(invokeMock).toHaveBeenCalledWith("preview_exclusions_cancel", {
      previewId: "gen-1",
    });
  });

  it("surfaces a start failure as a stable error code and stops scanning", async () => {
    const preview = createExclusionPreview();
    await preview.subscribe();
    invokeMock.mockRejectedValue({ code: "internal.invalid_input", message: "bad glob" });
    await preview.start({
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: ["["],
      excludePatterns: [],
    });
    expect(preview.errorCode.value).toBe("internal.invalid_input");
    expect(preview.scanning.value).toBe(false);
  });

  it("cancel stops the live walk and unsubscribing tears the listeners down", async () => {
    const preview = createExclusionPreview();
    const teardown = await preview.subscribe();
    invokeMock.mockResolvedValue("gen-1");
    await preview.start({
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    });

    teardown();
    expect(unlistenBatch).toHaveBeenCalled();
    expect(unlistenDone).toHaveBeenCalled();
    expect(invokeMock).toHaveBeenCalledWith("preview_exclusions_cancel", {
      previewId: "gen-1",
    });
    expect(preview.scanning.value).toBe(false);
  });

  it("attaches a child whose parent never streamed, rather than dropping it", async () => {
    // The backend streams breadth-first so this cannot normally happen; the
    // fallback exists so a node is never silently lost.
    const preview = await started();
    batchHandler!(batch("gen-1", [node("ghost/deep/leaf.txt", false, true, 2)]));
    preview.flush();

    expect(preview.roots.value.map((n) => n.path)).toEqual(["ghost"]);
    expect(preview.nodeAt("ghost")?.isDir).toBe(true);
    expect(preview.nodeAt("ghost/deep")?.children.map((n) => n.path)).toEqual([
      "ghost/deep/leaf.txt",
    ]);
  });

  it("updates a node in place when the backend re-sends it", async () => {
    const preview = await started();
    batchHandler!(batch("gen-1", [node("a.txt", false, true, 1)]));
    batchHandler!(batch("gen-1", [node("a.txt", false, false, 1)]));
    preview.flush();
    expect(preview.roots.value).toHaveLength(1);
    expect(preview.nodeAt("a.txt")?.included).toBe(false);
  });
});
