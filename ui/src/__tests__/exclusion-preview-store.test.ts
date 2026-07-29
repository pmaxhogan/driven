import { describe, it, expect, vi, beforeEach } from "vitest";

// Streaming exclusion-preview controller tests (SPEC s11.2; DESIGN s8.5 step 3).
// The seams are `@tauri-apps/api/core`'s `invoke` (the start / cancel commands)
// and `@tauri-apps/api/event`'s `listen` (the batch / done / error stream).
// Mocking both lets us drive the controller against a fake backend and fire
// events by hand, asserting: batches fold into a tree with parents before
// children, a superseded generation's events are discarded, the totals track the
// stream (and stay exact past a truncation), a recompute never blanks what is on
// screen, and the "+"/"-" globs match the Rust matcher's form.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

let batchHandler: ((payload: unknown) => void) | null = null;
let doneHandler: ((payload: unknown) => void) | null = null;
let errorHandler: ((payload: unknown) => void) | null = null;
const unlistenBatch = vi.fn();
const unlistenDone = vi.fn();
const unlistenError = vi.fn();
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
    if (event === "exclusion_preview:error") {
      errorHandler = (payload: unknown) => cb({ payload });
      return unlistenError;
    }
    return vi.fn();
  }),
}));

import {
  anchoredPatternForPath,
  appendPatternLine,
  createExclusionPreview,
  isUnconstrainedIncludePattern,
  unconstrainedIncludePatterns,
  PRE_ID_PARK_CAP,
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

/** Re-start `preview` under new rules, resolving to generation `id`. */
async function restart(preview: ExclusionPreviewController, id: string): Promise<void> {
  invokeMock.mockResolvedValue(id);
  await preview.start({
    sourceId: "src-1",
    respectGitignore: false,
    includePatterns: [],
    excludePatterns: ["*.txt"],
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  batchHandler = null;
  doneHandler = null;
  errorHandler = null;
  unlistenBatch.mockReset();
  unlistenDone.mockReset();
  unlistenError.mockReset();
});

describe("pre-generation-id park", () => {
  // A controller whose `start` never resolves a generation id leaves `currentId`
  // null FOREVER, while its `listen()` registrations - which are global by event
  // name - keep receiving every later preview's batches. Nothing drains the park
  // in that state, so the park is the one place in this controller where events
  // can pile up unboundedly. This pins the bound.
  it("caps the park so a controller that never resolves an id cannot grow without bound", async () => {
    const preview = createExclusionPreview();
    await preview.subscribe();
    // The start command REJECTS, so no generation id is ever adopted.
    invokeMock.mockRejectedValue(new Error("start failed"));
    await preview.start({
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    });
    expect(preview.currentPreviewId()).toBeNull();

    // Four caps' worth of traffic from OTHER previews still arrives, because the
    // listeners are registered by event name rather than per generation.
    const fired = PRE_ID_PARK_CAP * 4;
    for (let i = 0; i < fired; i += 1) {
      batchHandler!(batch(`gen-${i}`, [node(`f${i}.txt`, false, true, 1)]));
    }

    expect(preview.preIdParkedCount()).toBe(PRE_ID_PARK_CAP);
    expect(preview.preIdParkedCount()).toBeLessThan(fired);
  });

  it("still parks and replays everything that arrives before a real id lands", async () => {
    const preview = createExclusionPreview();
    await preview.subscribe();
    let resolveStart: (id: string) => void = () => {};
    invokeMock.mockReturnValue(
      new Promise<string>((resolve) => {
        resolveStart = resolve;
      })
    );
    const starting = preview.start({
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    });
    // The walk streams before `preview_exclusions_start` resolves.
    batchHandler!(batch("gen-1", [node("dir", true, true), node("dir/a.txt", false, true, 7)]));
    expect(preview.preIdParkedCount()).toBe(1);

    resolveStart("gen-1");
    await starting;
    preview.flush();

    expect(preview.preIdParkedCount()).toBe(0);
    expect(preview.nodeAt("dir/a.txt")?.included).toBe(true);
    expect(preview.includedCount.value).toBe(1);
  });
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

describe("isUnconstrainedIncludePattern", () => {
  // The scanner may only PRUNE an excluded directory when no include pattern
  // could match beneath it, which needs a root-anchored pattern of bounded
  // depth. Anything relative, or spanning levels with a double-star, forces the
  // walk into every excluded folder - those are the ones the editors warn about.
  const vectors: Array<[string, boolean]> = [
    // Unconstrained: no leading slash, so it matches at any depth.
    [".env", true],
    ["*/.env", true],
    ["blah/.env", true],
    ["**/.env", true],
    // Unconstrained: anchored, but a double-star spans any number of levels.
    ["/*/x/**/.env", true],
    ["/**", true],
    ["/a/**/b", true],
    // Constrained: anchored AND depth-bounded.
    ["/x/.env", false],
    ["/*/.env", false],
    ["/a/*/b/.env", false],
    ["/.env", false],
    ["/node_modules/", false],
    // Blank lines are not patterns at all.
    ["", false],
    ["   ", false],
  ];

  it.each(vectors)("treats %j as unconstrained=%s", (pattern, expected) => {
    expect(isUnconstrainedIncludePattern(pattern)).toBe(expected);
  });

  it("judges a pattern by its trimmed form", () => {
    // The editors trim each line before sending it, so surrounding whitespace
    // must not change the verdict either way.
    expect(isUnconstrainedIncludePattern("  /x/.env  ")).toBe(false);
    expect(isUnconstrainedIncludePattern("  .env  ")).toBe(true);
  });
});

describe("unconstrainedIncludePatterns", () => {
  it("reports only the offending patterns, in the order they were typed", () => {
    expect(unconstrainedIncludePatterns("/x/.env\n.env\n/a/*/b/.env\n/*/x/**/.env")).toEqual([
      ".env",
      "/*/x/**/.env",
    ]);
  });

  it("splits on commas as well as newlines, the way both editors do", () => {
    expect(unconstrainedIncludePatterns("/x/.env,blah/.env,/*/.env")).toEqual(["blah/.env"]);
  });

  it("ignores blank lines and surrounding whitespace", () => {
    expect(unconstrainedIncludePatterns("\n  \n  /x/.env  \n\n,, \t\n")).toEqual([]);
    expect(unconstrainedIncludePatterns("  */.env  \n\n")).toEqual(["*/.env"]);
  });

  it("is empty for empty input and for an all-anchored list", () => {
    expect(unconstrainedIncludePatterns("")).toEqual([]);
    expect(unconstrainedIncludePatterns("/x/.env\n/*/.env\n/a/*/b/.env")).toEqual([]);
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

  it("keeps the previous result on screen until the new generation replaces it", async () => {
    // The reset-flash this fixes: a rule edit used to blank the tree and zero
    // the counts the instant it started, so every keystroke flashed the panel
    // empty before repopulating it.
    const preview = await started();
    batchHandler!(batch("gen-1", [node("old.txt", false, true, 9)]));
    preview.flush();
    expect(preview.roots.value.map((n) => n.path)).toEqual(["old.txt"]);

    await restart(preview, "gen-2");
    expect(preview.currentPreviewId()).toBe("gen-2");
    expect(preview.roots.value.map((n) => n.path)).toEqual([
      "old.txt",
      // the previous answer is still the best one available
    ]);
    expect(preview.includedCount.value).toBe(1);
    expect(preview.includedBytes.value).toBe(9);
    expect(preview.recomputing.value).toBe(true);
    expect(preview.scanning.value).toBe(true);

    // The OLD generation's still-in-flight events must not fold into the new
    // tree even while the old tree is what is being rendered.
    batchHandler!(batch("gen-1", [node("sneaky.txt", false, true, 1)]));
    preview.flush();
    expect(preview.roots.value.map((n) => n.path)).toEqual(["old.txt"]);

    // The new generation's FIRST batch swaps the whole thing over at once.
    batchHandler!(batch("gen-2", [node("new.txt", false, true, 4)]));
    preview.flush();
    expect(preview.roots.value.map((n) => n.path)).toEqual(["new.txt"]);
    expect(preview.nodeAt("old.txt")).toBeUndefined();
    expect(preview.includedCount.value).toBe(1);
    expect(preview.includedBytes.value).toBe(4);
    expect(preview.recomputing.value).toBe(false);
  });

  it("never lets the totals dip through zero on the way to the new ones", async () => {
    // The user watches these numbers to judge a rule. Bouncing them through 0
    // reads as "your pattern just excluded everything" - so every observed
    // value must be either the old answer or the new one, never a blank.
    const preview = await started();
    batchHandler!(
      batch("gen-1", [node("a.txt", false, true, 100)], {
        includedCount: 40,
        excludedCount: 2,
        includedBytes: 4000,
      })
    );
    preview.flush();

    const seen: number[] = [];
    const record = (): void => {
      seen.push(preview.includedCount.value);
    };

    record();
    await restart(preview, "gen-2");
    record();
    preview.flush();
    record();
    batchHandler!(
      batch("gen-2", [node("a.txt", false, true, 100)], {
        includedCount: 31,
        excludedCount: 11,
        includedBytes: 3100,
      })
    );
    preview.flush();
    record();

    expect(seen).toEqual([40, 40, 40, 31]);
    expect(preview.excludedCount.value).toBe(11);
    expect(preview.includedBytes.value).toBe(3100);
  });

  it("swaps on the new generation's done even when it found nothing", async () => {
    // A `done` is the generation's final word. If the folder really is empty
    // under the new rules, holding the old tree forever would be a lie.
    const preview = await started();
    batchHandler!(batch("gen-1", [node("old.txt", false, true, 9)]));
    preview.flush();

    await restart(preview, "gen-2");
    doneHandler!({
      previewId: "gen-2",
      includedCount: 0,
      excludedCount: 0,
      includedBytes: 0,
      truncated: false,
      cancelled: false,
    });
    preview.flush();

    expect(preview.roots.value).toHaveLength(0);
    expect(preview.includedCount.value).toBe(0);
    expect(preview.recomputing.value).toBe(false);
    expect(preview.complete.value).toBe(true);
  });

  it("does not swap to an empty tree for a generation that was abandoned", async () => {
    // A cancelled generation produced nothing and never will. Blanking the
    // panel on its behalf would flash exactly what this avoids.
    const preview = await started();
    batchHandler!(batch("gen-1", [node("old.txt", false, true, 9)]));
    preview.flush();

    await restart(preview, "gen-2");
    doneHandler!({
      previewId: "gen-2",
      includedCount: 0,
      excludedCount: 0,
      includedBytes: 0,
      truncated: false,
      cancelled: true,
    });
    preview.flush();

    expect(preview.roots.value.map((n) => n.path)).toEqual(["old.txt"]);
    expect(preview.includedCount.value).toBe(1);
    expect(preview.scanning.value).toBe(false);
    expect(preview.recomputing.value).toBe(false);
    expect(preview.complete.value).toBe(false);
  });

  it("publishes the first preview's content as soon as its first batch lands", async () => {
    // Nothing to preserve on a first open, so there is no swap to wait for -
    // rows must render on batch 1 rather than at the end of the walk.
    const preview = await started();
    expect(preview.recomputing.value).toBe(false);
    expect(preview.scanning.value).toBe(true);
    expect(preview.roots.value).toHaveLength(0);

    batchHandler!(batch("gen-1", [node("first.txt", false, true, 3)]));
    preview.flush();
    expect(preview.roots.value.map((n) => n.path)).toEqual(["first.txt"]);
    expect(preview.includedCount.value).toBe(1);
    expect(preview.scanning.value).toBe(true);
  });

  it("surfaces a post-start error event as a stable code and stops the spinner", async () => {
    // The backend hands out the generation id before it knows the preview is
    // viable (building the matcher reads ignore files off disk), so a setup
    // failure arrives as an event rather than as a rejected start.
    const preview = await started();
    errorHandler!({
      previewId: "gen-1",
      code: "local.io_error",
      message: "adding exclude `[`: unclosed class",
    });
    expect(preview.errorCode.value).toBe("local.io_error");
    expect(preview.scanning.value).toBe(false);
    expect(preview.recomputing.value).toBe(false);
  });

  it("ignores an error event from a superseded generation", async () => {
    const preview = await started();
    errorHandler!({ previewId: "gen-STALE", code: "internal.bug", message: "boom" });
    expect(preview.errorCode.value).toBeNull();
    expect(preview.scanning.value).toBe(true);
  });

  it("replays a pre-id error event once the generation id resolves", async () => {
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

    errorHandler!({ previewId: "gen-1", code: "local.io_error", message: "nope" });
    errorHandler!({ previewId: "gen-OTHER", code: "internal.bug", message: "not mine" });
    resolveStart("gen-1");
    await starting;

    expect(preview.errorCode.value).toBe("local.io_error");
    expect(preview.scanning.value).toBe(false);
  });

  it("clears a previous error when the next generation starts", async () => {
    const preview = await started();
    errorHandler!({ previewId: "gen-1", code: "local.io_error", message: "nope" });
    expect(preview.errorCode.value).toBe("local.io_error");

    await restart(preview, "gen-2");
    expect(preview.errorCode.value).toBeNull();
    expect(preview.scanning.value).toBe(true);
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
    expect(unlistenError).toHaveBeenCalled();
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
