// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";

// Mount tests for the exclusion editor's live folder tree (SPEC s11.2; DESIGN
// s8.5 step 3). They drive the real component against a faked backend (the
// `invoke` seam) and hand-fired streaming events (the `listen` seam), asserting
// the four things the feature promises: rows stream in while the scan runs,
// included vs excluded is distinguishable without relying on colour, folders
// start COLLAPSED, and each row's "+"/"-" emits the matcher-verified glob and
// re-runs the scan.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

let batchHandler: ((payload: unknown) => void) | null = null;
let doneHandler: ((payload: unknown) => void) | null = null;
let errorHandler: ((payload: unknown) => void) | null = null;
/** Every unlisten handed out by `listen()`, so a test can assert that NONE is
 *  left registered after the component goes away. */
const unlistenSpies: Array<ReturnType<typeof vi.fn>> = [];
/** When set, `listen()` awaits this before resolving - which is what lets a test
 *  unmount the component while `subscribe()` is still in flight. */
let listenGate: Promise<void> | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
    if (event === "exclusion_preview:batch") {
      batchHandler = (payload: unknown) => cb({ payload });
    }
    if (event === "exclusion_preview:done") {
      doneHandler = (payload: unknown) => cb({ payload });
    }
    if (event === "exclusion_preview:error") {
      errorHandler = (payload: unknown) => cb({ payload });
    }
    if (listenGate) await listenGate;
    const unlisten = vi.fn();
    unlistenSpies.push(unlisten);
    return unlisten;
  }),
}));

import { i18n } from "../i18n";
import ExclusionPreviewTree from "../components/ExclusionPreviewTree.vue";
import type { ExclusionPreviewBatch, ExclusionPreviewNode } from "../ipc/types";

const globalMountOptions = { plugins: [i18n] };

function node(
  path: string,
  isDir: boolean,
  included: boolean,
  size = 0,
  fileCount = 0,
  byteSize = 0
): ExclusionPreviewNode {
  return { path, isDir, included, size, fileCount, byteSize };
}

function batch(nodes: ExclusionPreviewNode[], previewId = "gen-1"): ExclusionPreviewBatch {
  const files = nodes.filter((n) => !n.isDir);
  return {
    previewId,
    nodes,
    includedCount: files.filter((n) => n.included).length,
    excludedCount: files.filter((n) => !n.included).length,
    includedBytes: files.filter((n) => n.included).reduce((a, n) => a + n.size, 0),
    excludedBytes: files.filter((n) => !n.included).reduce((a, n) => a + n.size, 0),
    truncated: false,
  };
}

/** Mount the tree, let it start its walk, and stream `nodes` into it. */
async function mountWithNodes(nodes: ExclusionPreviewNode[]) {
  const wrapper = mount(ExclusionPreviewTree, {
    global: globalMountOptions,
    props: {
      sourceId: "src-1",
      respectGitignore: true,
      includePatterns: [],
      excludePatterns: [],
    },
  });
  await flushPromises();
  batchHandler!(batch(nodes));
  // The controller coalesces batches into one flush per animation frame; jsdom
  // has no rAF driver here, so advance the fallback timer.
  vi.advanceTimersByTime(20);
  await flushPromises();
  return wrapper;
}

/** Let the controller's rAF-fallback flush run, then settle Vue. */
async function settle(): Promise<void> {
  vi.advanceTimersByTime(20);
  await flushPromises();
}

beforeEach(() => {
  vi.useFakeTimers();
  invokeMock.mockReset();
  invokeMock.mockResolvedValue("gen-1");
  batchHandler = null;
  doneHandler = null;
  errorHandler = null;
  unlistenSpies.length = 0;
  listenGate = null;
});

describe("ExclusionPreviewTree teardown", () => {
  it("tears down every listener when unmounted while subscribe is still in flight", async () => {
    // The editor mounts under a `v-if` (SourceTable's inline editor,
    // AddSourceWizard's exclusions step), so opening and immediately closing it
    // is ordinary use. `subscribe()` is three async `listen()` round-trips; if
    // the component unmounts inside that window, the resolved unlisteners must
    // still be invoked. They cannot be recovered later: `listen` registers
    // GLOBALLY BY EVENT NAME, so a stranded set keeps receiving every later
    // preview's batches and parks them in a controller nobody can reach.
    let openGate: () => void = () => {};
    listenGate = new Promise<void>((resolve) => {
      openGate = resolve;
    });

    const wrapper = mount(ExclusionPreviewTree, {
      global: globalMountOptions,
      props: {
        sourceId: "src-1",
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
      },
    });
    // Nothing has resolved yet - this is the race window.
    expect(unlistenSpies).toHaveLength(0);

    wrapper.unmount();
    openGate();
    await flushPromises();

    expect(unlistenSpies).toHaveLength(3);
    for (const unlisten of unlistenSpies) {
      expect(unlisten).toHaveBeenCalledTimes(1);
    }
    // ...and no walk is started for a tree that is no longer on screen.
    expect(invokeMock).not.toHaveBeenCalledWith("preview_exclusions_start", expect.anything());
  });

  it("tears down every listener on an ordinary unmount", async () => {
    const wrapper = await mountWithNodes([node("a.txt", false, true, 10)]);
    expect(unlistenSpies).toHaveLength(3);

    wrapper.unmount();
    await flushPromises();

    for (const unlisten of unlistenSpies) {
      expect(unlisten).toHaveBeenCalledTimes(1);
    }
  });
});

describe("ExclusionPreviewTree", () => {
  it("starts a streaming preview on mount and shows the scanning indicator", async () => {
    const wrapper = await mountWithNodes([node("a.txt", false, true, 10)]);

    expect(invokeMock).toHaveBeenCalledWith(
      "preview_exclusions_start",
      expect.objectContaining({ req: expect.objectContaining({ sourceId: "src-1" }) })
    );
    // (a) the scan is visibly still running while rows already render.
    expect(wrapper.find('[data-testid="preview-scanning"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="preview-complete"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="preview-row-a.txt"]').exists()).toBe(true);
  });

  // Issue #305: per-folder rollups.
  it("shows a folder row's file-count and byte rollup, right of the name", async () => {
    const wrapper = await mountWithNodes([
      node("node_modules", true, false, 0, 31_204, 2_254_857_830),
    ]);
    const row = wrapper.get('[data-testid="preview-row-node_modules"]');
    expect(row.text()).toContain(
      i18n.global.t("settings.exclusionPreview.rollup", { count: "31,204", size: "2.1 GB" })
    );
    const rollup = wrapper.get('[data-testid="preview-rollup-node_modules"]');
    expect(rollup.text()).toBe(
      i18n.global.t("settings.exclusionPreview.rollup", { count: "31,204", size: "2.1 GB" })
    );
  });

  it("settles a folder's rollup across later batches without duplicating the row", async () => {
    const wrapper = await mountWithNodes([node("docs", true, true, 0, 0, 0)]);
    expect(wrapper.get('[data-testid="preview-rollup-docs"]').text()).toContain("0 files");

    batchHandler!(batch([node("docs", true, true, 0, 5, 500)]));
    await settle();

    expect(wrapper.findAll('[data-testid="preview-row-docs"]')).toHaveLength(1);
    expect(wrapper.get('[data-testid="preview-rollup-docs"]').text()).toContain(
      i18n.global.t("settings.exclusionPreview.rollup", { count: "5", size: "500 B" })
    );
  });

  it("shows the would-be-freed total for excluded bytes in the summary line", async () => {
    const wrapper = mount(ExclusionPreviewTree, {
      global: globalMountOptions,
      props: {
        sourceId: "src-1",
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
      },
    });
    await flushPromises();
    batchHandler!({
      ...batch([node("skip.log", false, false, 1024)]),
      excludedBytes: 1024,
    });
    vi.advanceTimersByTime(20);
    await flushPromises();

    expect(wrapper.get('[data-testid="preview-excluded-bytes"]').text()).toBe(
      i18n.global.t("settings.addSource.preview.excludedBytes", { size: "1 KB" })
    );
  });

  it("flexes to fill its container when `fill` is set, instead of a fixed cap", async () => {
    const capped = await mountWithNodes([node("a.txt", false, true)]);
    expect(capped.get('[data-testid="exclusion-preview"]').classes()).not.toContain("flex-col");
    const cappedTree = capped.get('[role="tree"]').element.parentElement;
    expect(cappedTree?.className).toContain("max-h-64");

    const filled = mount(ExclusionPreviewTree, {
      global: globalMountOptions,
      props: {
        sourceId: "src-1",
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
        fill: true,
      },
    });
    await flushPromises();
    batchHandler!(batch([node("a.txt", false, true)]));
    vi.advanceTimersByTime(20);
    await flushPromises();

    // The root becomes a flex column in fill mode...
    expect(filled.get('[data-testid="exclusion-preview"]').classes()).toContain("flex-col");
    // ...and the old fixed cap on the scrollable tree body is gone, replaced
    // by flex-fill sizing.
    const filledTree = filled.get('[role="tree"]').element.parentElement;
    expect(filledTree?.className).not.toContain("max-h-64");
    expect(filledTree?.className).toContain("flex-1");
  });

  it("renders streamed rows with live counts while the walk is in flight", async () => {
    const wrapper = await mountWithNodes([
      node("docs", true, true),
      node("keep.txt", false, true, 2048),
      node("skip.log", false, false, 10),
    ]);

    const text = wrapper.text();
    expect(text).toContain("docs");
    expect(text).toContain("keep.txt");
    expect(text).toContain("skip.log");
    // The counts are FILE counts - the streamed `docs` directory is a tree row,
    // not a backed-up file.
    expect(text).toContain(i18n.global.t("settings.addSource.preview.included", { count: "1" }));
    expect(text).toContain(i18n.global.t("settings.addSource.preview.excluded", { count: "1" }));
    expect(text).toContain("2 KB");
  });

  it("switches to the complete state on the done event", async () => {
    const wrapper = await mountWithNodes([node("a.txt", false, true)]);
    doneHandler!({
      previewId: "gen-1",
      includedCount: 1,
      excludedCount: 0,
      includedBytes: 4,
      excludedBytes: 0,
      truncated: false,
      cancelled: false,
    });
    vi.advanceTimersByTime(20);
    await flushPromises();

    expect(wrapper.find('[data-testid="preview-scanning"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="preview-complete"]').exists()).toBe(true);
  });

  it("shows the truncation notice while keeping the counts on screen", async () => {
    const wrapper = mount(ExclusionPreviewTree, {
      global: globalMountOptions,
      props: {
        sourceId: "src-1",
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
      },
    });
    await flushPromises();
    batchHandler!({
      ...batch([node("a.txt", false, true)]),
      includedCount: 60_000,
      truncated: true,
    });
    vi.advanceTimersByTime(20);
    await flushPromises();

    expect(wrapper.find('[data-testid="preview-truncated"]').exists()).toBe(true);
    expect(wrapper.text()).toContain(
      i18n.global.t("settings.addSource.preview.included", { count: "60,000" })
    );
  });

  // (b) included vs excluded is distinguishable WITHOUT colour.
  it("marks an excluded row with a strikethrough and a text label, not colour alone", async () => {
    const wrapper = await mountWithNodes([
      node("keep.txt", false, true),
      node("skip.log", false, false),
    ]);

    const included = wrapper.get('[data-testid="preview-row-keep.txt"]');
    const excluded = wrapper.get('[data-testid="preview-row-skip.log"]');
    expect(excluded.html()).toContain("line-through");
    expect(included.html()).not.toContain("line-through");
    expect(excluded.text()).toContain(i18n.global.t("settings.exclusionPreview.excludedLabel"));
    expect(included.text()).toContain(i18n.global.t("settings.exclusionPreview.includedLabel"));
    // The two verdicts also use different glyphs, so the cue survives greyscale.
    expect(included.find("svg").html()).not.toBe(excluded.find("svg").html());
  });

  // (c) every folder starts COLLAPSED.
  it("starts every folder collapsed and reveals children only on expand", async () => {
    const wrapper = await mountWithNodes([
      node("docs", true, true),
      node("docs/inner.txt", false, true),
      node("docs/nested", true, true),
      node("docs/nested/deep.txt", false, true),
    ]);

    // The root's immediate children are visible as collapsed rows...
    const folderRow = wrapper.get('[data-testid="preview-row-docs"]');
    expect(folderRow.attributes("aria-expanded")).toBe("false");
    // ...and nothing beneath them is in the DOM at all.
    expect(wrapper.find('[data-testid="preview-row-docs/inner.txt"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="preview-row-docs/nested"]').exists()).toBe(false);

    await folderRow.get("button").trigger("click");
    expect(wrapper.get('[data-testid="preview-row-docs"]').attributes("aria-expanded")).toBe(
      "true"
    );
    expect(wrapper.find('[data-testid="preview-row-docs/inner.txt"]').exists()).toBe(true);
    // The nested folder is itself still collapsed - expanding is not recursive.
    const nested = wrapper.get('[data-testid="preview-row-docs/nested"]');
    expect(nested.attributes("aria-expanded")).toBe("false");
    expect(wrapper.find('[data-testid="preview-row-docs/nested/deep.txt"]').exists()).toBe(false);

    await nested.get("button").trigger("click");
    expect(wrapper.find('[data-testid="preview-row-docs/nested/deep.txt"]').exists()).toBe(true);

    // Collapsing removes the subtree from the DOM again.
    await wrapper.get('[data-testid="preview-row-docs"]').get("button").trigger("click");
    expect(wrapper.find('[data-testid="preview-row-docs/inner.txt"]').exists()).toBe(false);
  });

  it("exposes the tree with the ARIA roles a screen reader needs", async () => {
    const wrapper = await mountWithNodes([
      node("docs", true, true),
      node("docs/a.txt", false, true),
    ]);
    const tree = wrapper.get('[role="tree"]');
    expect(tree.attributes("aria-label")).toBe(
      i18n.global.t("settings.exclusionPreview.treeLabel")
    );
    const row = wrapper.get('[data-testid="preview-row-docs"]');
    expect(row.attributes("role")).toBe("treeitem");
    expect(row.attributes("aria-level")).toBe("1");

    await row.get("button").trigger("click");
    const child = wrapper.get('[data-testid="preview-row-docs/a.txt"]');
    expect(child.attributes("aria-level")).toBe("2");
    // A file is not expandable, so it carries no aria-expanded at all.
    expect(child.attributes("aria-expanded")).toBeUndefined();
  });

  // (d) the per-row "+" / "-" actions.
  it("emits an EXCLUDE glob for an included row and re-runs the scan", async () => {
    const wrapper = await mountWithNodes([node("docs", true, true), node("keep.txt", false, true)]);
    invokeMock.mockClear();

    await wrapper.get('[data-testid="preview-action-keep.txt"]').trigger("click");
    await flushPromises();

    expect(wrapper.emitted("append-exclude")).toEqual([["/keep.txt"]]);
    expect(wrapper.emitted("append-include")).toBeUndefined();
    // The click re-classifies immediately rather than waiting for a blur.
    expect(invokeMock).toHaveBeenCalledWith("preview_exclusions_start", expect.anything());
  });

  it("emits an INCLUDE glob for an excluded row", async () => {
    const wrapper = await mountWithNodes([node("logs/keep.log", false, false)]);
    await wrapper.get('[data-testid="preview-row-logs"]').get("button").trigger("click");

    await wrapper.get('[data-testid="preview-action-logs/keep.log"]').trigger("click");
    await flushPromises();

    expect(wrapper.emitted("append-include")).toEqual([["/logs/keep.log"]]);
    expect(wrapper.emitted("append-exclude")).toBeUndefined();
  });

  it("emits a DIRECTORY glob (trailing slash) for a folder row", async () => {
    const wrapper = await mountWithNodes([node("build", true, true)]);

    await wrapper.get('[data-testid="preview-action-build"]').trigger("click");
    await flushPromises();

    // The trailing slash is what makes the matcher apply it to the whole
    // subtree; the Rust exclude tests pin that behaviour.
    expect(wrapper.emitted("append-exclude")).toEqual([["/build/"]]);
  });

  it("labels each action button with the path it targets", async () => {
    const wrapper = await mountWithNodes([
      node("keep.txt", false, true),
      node("skip.log", false, false),
    ]);
    expect(wrapper.get('[data-testid="preview-action-keep.txt"]').attributes("aria-label")).toBe(
      i18n.global.t("settings.exclusionPreview.excludeAction", { path: "keep.txt" })
    );
    expect(wrapper.get('[data-testid="preview-action-skip.log"]').attributes("aria-label")).toBe(
      i18n.global.t("settings.exclusionPreview.includeAction", { path: "skip.log" })
    );
  });

  it("withholds the action on a path no single glob can express", async () => {
    // A newline in a filename cannot be a pattern LINE, so offering the button
    // would append a rule that silently matches something else.
    const wrapper = await mountWithNodes([node("we\nird.txt", false, true)]);
    // The row itself still renders (the file is real and its verdict matters)...
    expect(wrapper.findAll('[role="treeitem"]')).toHaveLength(1);
    expect(wrapper.text()).toContain("ird.txt");
    // ...it just gets no action button.
    expect(wrapper.findAll('[data-testid^="preview-action-"]')).toHaveLength(0);
  });

  it("pages a huge directory behind a show-more row instead of mounting it all", async () => {
    const many = Array.from({ length: 260 }, (_, i) =>
      node(`f${String(i).padStart(3, "0")}.txt`, false, true)
    );
    const wrapper = await mountWithNodes(many);

    // 200 rows rendered, not 260 - the DOM stays bounded however wide the tree.
    expect(wrapper.findAll('[role="treeitem"]')).toHaveLength(200);
    const showMore = wrapper
      .findAll("button")
      .find(
        (b) => b.text() === i18n.global.t("settings.exclusionPreview.showMore", { count: "60" })
      );
    expect(showMore).toBeDefined();

    await showMore!.trigger("click");
    expect(wrapper.findAll('[role="treeitem"]')).toHaveLength(260);
  });

  it("re-runs the walk when the parent calls restart after a rule edit", async () => {
    const wrapper = await mountWithNodes([node("a.txt", false, true)]);
    invokeMock.mockClear();

    await wrapper.setProps({ excludePatterns: ["*.txt"] });
    await (wrapper.vm as unknown as { restart: () => Promise<void> }).restart();
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith(
      "preview_exclusions_start",
      expect.objectContaining({
        req: expect.objectContaining({ excludePatterns: ["*.txt"] }),
      })
    );
  });

  it("cancels the in-flight walk when the editor unmounts", async () => {
    const wrapper = await mountWithNodes([node("a.txt", false, true)]);
    invokeMock.mockClear();
    wrapper.unmount();
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("preview_exclusions_cancel", {
      previewId: "gen-1",
    });
  });

  // (e) a rule edit updates the tree in place - it never blanks it.
  it("keeps the old rows and counts visible, dimmed, while a rule edit recomputes", async () => {
    const wrapper = await mountWithNodes([
      node("keep.txt", false, true, 1024),
      node("skip.log", false, false, 8),
    ]);
    expect(wrapper.find('[data-testid="preview-recomputing"]').exists()).toBe(false);

    invokeMock.mockResolvedValue("gen-2");
    await wrapper.setProps({ excludePatterns: ["*.log"] });
    await (wrapper.vm as unknown as { restart: () => Promise<void> }).restart();
    await settle();

    // The previous answer is still on screen and still readable - just marked
    // as being refreshed. Blanking it here is the bug this replaces.
    expect(wrapper.find('[data-testid="preview-row-keep.txt"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="preview-row-skip.log"]').exists()).toBe(true);
    expect(wrapper.text()).toContain(
      i18n.global.t("settings.addSource.preview.included", { count: "1" })
    );
    expect(wrapper.text()).toContain("1 KB");
    const recomputing = wrapper.get('[data-testid="preview-recomputing"]');
    expect(recomputing.attributes("role")).toBe("status");
    expect(recomputing.text()).toBe(i18n.global.t("settings.exclusionPreview.recomputing"));
    // ...and only ONE state badge shows at a time.
    expect(wrapper.find('[data-testid="preview-scanning"]').exists()).toBe(false);
    expect(wrapper.get('[role="tree"]').element.closest(".opacity-60")).not.toBeNull();

    // The new generation's first batch swaps everything over at once.
    batchHandler!(batch([node("keep.txt", false, true, 1024)], "gen-2"));
    await settle();
    expect(wrapper.find('[data-testid="preview-recomputing"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="preview-scanning"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="preview-row-skip.log"]').exists()).toBe(false);
    expect(wrapper.get('[role="tree"]').element.closest(".opacity-60")).toBeNull();
  });

  it("keeps the folders the user opened expanded across a rule edit", async () => {
    // A rule edit re-classifies the SAME folder, so collapsing the tree throws
    // away the user's place in it right when they are inspecting a subtree.
    const wrapper = await mountWithNodes([
      node("docs", true, true),
      node("docs/inner.txt", false, true),
    ]);
    await wrapper.get('[data-testid="preview-row-docs"]').get("button").trigger("click");
    expect(wrapper.find('[data-testid="preview-row-docs/inner.txt"]').exists()).toBe(true);

    invokeMock.mockResolvedValue("gen-2");
    await wrapper.setProps({ excludePatterns: ["*.txt"] });
    await (wrapper.vm as unknown as { restart: () => Promise<void> }).restart();
    batchHandler!(batch([node("docs", true, true), node("docs/inner.txt", false, false)], "gen-2"));
    await settle();

    expect(wrapper.get('[data-testid="preview-row-docs"]').attributes("aria-expanded")).toBe(
      "true"
    );
    const inner = wrapper.get('[data-testid="preview-row-docs/inner.txt"]');
    expect(inner.html()).toContain("line-through");
  });

  it("localizes a post-start error event the same way as a rejected start", async () => {
    const wrapper = await mountWithNodes([node("a.txt", false, true)]);
    errorHandler!({ previewId: "gen-1", code: "local.io_error", message: "unreadable" });
    await settle();

    const alert = wrapper.get('[role="alert"]');
    expect(alert.text()).toBe(i18n.global.t("errors.local.io_error.long"));
    expect(wrapper.find('[role="tree"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="preview-scanning"]').exists()).toBe(false);
  });

  it("localizes a start failure instead of leaving an empty tree unexplained", async () => {
    invokeMock.mockRejectedValue({ code: "internal.invalid_input", message: "bad glob" });
    const wrapper = mount(ExclusionPreviewTree, {
      global: globalMountOptions,
      props: {
        sourceId: "src-1",
        respectGitignore: true,
        includePatterns: ["["],
        excludePatterns: [],
      },
    });
    await flushPromises();

    const alert = wrapper.get('[role="alert"]');
    expect(alert.text()).toBe(i18n.global.t("errors.internal.invalid_input.long"));
    expect(wrapper.find('[role="tree"]').exists()).toBe(false);
  });
});
