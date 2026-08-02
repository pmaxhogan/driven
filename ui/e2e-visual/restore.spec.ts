import {
  REMOTE_TREE_EMPTY,
  RESTORE_JOB_DONE,
  RESTORE_JOB_RUNNING,
  SOURCE_DOCUMENTS,
} from "../test-support/fixtures";
import { mockError, mockPending } from "../test-support/mock-backend";
import { expect, snapshot, test } from "./fixtures";

// The Restore browser (DESIGN s8.4). `/restore/:sourceId` scopes straight to
// one source, which is also the only way to get a populated tree without
// driving the source dropdown.

const SCOPED = `/restore/${SOURCE_DOCUMENTS.id}`;

test("browsing a source", async ({ page, visit }) => {
  await visit(SCOPED);
  await expect(page.getByTestId("restore-list")).toBeVisible();
  await snapshot(page, "browse.png");
});

test("files selected, ready to restore", async ({ page, visit }) => {
  await visit(SCOPED);
  await expect(page.getByTestId("restore-list")).toBeVisible();
  // Selecting reveals the action bar (destination picker, as-of date, Restore).
  await page.getByRole("checkbox", { name: "Select file to restore" }).first().check();
  await expect(page.getByTestId("restore-action-bar")).toBeVisible();
  await snapshot(page, "selection.png");
});

test("search results", async ({ page, visit }) => {
  await visit(SCOPED);
  await expect(page.getByTestId("restore-list")).toBeVisible();
  await page.getByTestId("restore-search-input").fill("*.pdf");
  await page.getByTestId("restore-search-input").press("Enter");
  await expect(page.getByText("reports/2026-q1.pdf")).toBeVisible();
  await snapshot(page, "search.png");
});

test("search with no matches", async ({ page, visit }) => {
  await visit(SCOPED, { commands: { search_files: [] } });
  await expect(page.getByTestId("restore-list")).toBeVisible();
  await page.getByTestId("restore-search-input").fill("nothing-matches-this");
  await page.getByTestId("restore-search-input").press("Enter");
  await expect(page.getByTestId("restore-empty")).toBeVisible();
  await snapshot(page, "search-empty.png");
});

test("an empty folder", async ({ page, visit }) => {
  await visit(SCOPED, { commands: { list_remote_tree: REMOTE_TREE_EMPTY } });
  await expect(page.getByTestId("restore-empty")).toBeVisible();
  await snapshot(page, "empty-folder.png");
});

test("no sources to restore from", async ({ page, visit }) => {
  await visit("/restore", { commands: { list_sources: [] } });
  await expect(page.getByRole("heading", { name: "Restore", level: 1 })).toBeVisible();
  await snapshot(page, "no-sources.png");
});

test("loading the tree", async ({ page, visit }) => {
  await visit(SCOPED, { commands: { list_remote_tree: mockPending() } });
  await expect(page.getByRole("heading", { name: "Restore", level: 1 })).toBeVisible();
  await snapshot(page, "loading.png");
});

test("the tree fails to load", async ({ page, visit }) => {
  await visit(SCOPED, { commands: { list_remote_tree: mockError("state.db_locked") } });
  await expect(page.getByRole("heading", { name: "Restore", level: 1 })).toBeVisible();
  await snapshot(page, "error.png");
});

// The job panel is event-driven: `restore:progress` carries every update. These
// two drive it through the mock's `emit` rather than by starting a real job, so
// the exact progress state under test is the one that gets captured.

test("a restore job in progress", async ({ page, visit }) => {
  await visit(SCOPED);
  await expect(page.getByTestId("restore-list")).toBeVisible();
  const notified = await page.evaluate(
    (status) => window.__drivenMock?.emit("restore:progress", status) ?? 0,
    RESTORE_JOB_RUNNING
  );
  // A zero here would mean the view had not subscribed yet and the screenshot
  // would silently capture the idle state instead.
  expect(notified).toBeGreaterThan(0);
  await expect(page.getByText("Restoring...")).toBeVisible();
  await snapshot(page, "job-running.png");
});

test("a finished restore job", async ({ page, visit }) => {
  await visit(SCOPED);
  await expect(page.getByTestId("restore-list")).toBeVisible();
  await page.evaluate(
    (status) => window.__drivenMock?.emit("restore:progress", status),
    RESTORE_JOB_DONE
  );
  await expect(page.getByText("Restore complete")).toBeVisible();
  await snapshot(page, "job-done.png");
});
