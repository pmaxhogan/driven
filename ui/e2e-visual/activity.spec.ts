import {
  ACTIVITY_PAGE_EMPTY,
  ACTIVITY_SUMMARY_EMPTY,
  ACTIVITY_THROUGHPUT_EMPTY,
} from "../test-support/fixtures";
import { mockError, mockPending } from "../test-support/mock-backend";
import { expect, snapshot, test } from "./fixtures";

// The Activity dashboard (DESIGN s8.3): header aggregates + sparkline tiles,
// the integrity-scrub panel, the restore-drill panel, the filter row and the
// event table.

const HEADING = { name: "Activity", level: 1 } as const;

test("populated", async ({ page, visit }) => {
  await visit("/activity");
  // The row-count summary only renders once a page of activity has landed, so
  // it is the readiness signal for the whole table.
  await expect(page.getByText("Showing 12 of 12 events")).toBeVisible();
  await snapshot(page, "populated.png");
});

test("empty", async ({ page, visit }) => {
  // NOTE: still one account. Zero accounts would send the router's first-run
  // guard to /setup and screenshot the wizard instead.
  await visit("/activity", {
    commands: {
      query_activity: ACTIVITY_PAGE_EMPTY,
      activity_summary: ACTIVITY_SUMMARY_EMPTY,
      activity_throughput_series: ACTIVITY_THROUGHPUT_EMPTY,
      distinct_activity_event_types: [],
      list_scrub_runs: [],
      list_drill_runs: [],
      list_sources: [],
    },
  });
  await expect(page.getByRole("heading", HEADING)).toBeVisible();
  await snapshot(page, "empty.png");
});

test("loading skeletons", async ({ page, visit }) => {
  await visit("/activity", {
    commands: {
      query_activity: mockPending(),
      activity_summary: mockPending(),
      activity_throughput_series: mockPending(),
      list_scrub_runs: mockPending(),
      list_drill_runs: mockPending(),
    },
  });
  await expect(page.getByRole("heading", HEADING)).toBeVisible();
  await snapshot(page, "loading.png");
});

test("query failure", async ({ page, visit }) => {
  await visit("/activity", {
    commands: {
      query_activity: mockError("state.db_locked"),
      activity_summary: mockError("state.db_locked"),
      activity_throughput_series: mockError("state.db_locked"),
      list_scrub_runs: mockError("state.db_locked"),
      list_drill_runs: mockError("state.db_locked"),
    },
  });
  await expect(page.getByRole("heading", HEADING)).toBeVisible();
  await snapshot(page, "error.png");
});
