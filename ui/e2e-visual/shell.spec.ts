import {
  ACCOUNT_DRIVE,
  ACCOUNT_S3,
  FIXED_NOW,
  SYNC_STATUS_RUNNING,
  UPDATE_INFO,
  WORK_QUEUE_BUSY,
} from "../test-support/fixtures";
import { expect, snapshot, test } from "./fixtures";

// The app shell (App.vue): the sticky top chrome - global progress bar, paused
// banner, gate-reason status banner, update banner, top nav - plus the toast
// host. All of it is app-lifetime and renders over EVERY route, so a regression
// here is a regression everywhere. Activity is the backdrop throughout.

test("idle shell", async ({ page, visit }) => {
  await visit("/activity");
  await expect(page.getByText("Showing 12 of 12 events")).toBeVisible();
  await snapshot(page, "idle.png");
});

test("a backup is running", async ({ page, visit }) => {
  await visit("/activity", { commands: { get_sync_status: SYNC_STATUS_RUNNING } });
  await expect(page.getByText("Showing 12 of 12 events")).toBeVisible();
  // Issue #270. The activity table above is NOT a readiness signal for the
  // progress bar, which is the whole point of this scenario: the table comes
  // from `query_activity`, while the bar belongs to the progress store
  // (`get_sync_status`, hydrated separately during app boot) and its visibility
  // additionally runs through a deliberate 500ms debounce - `useDebouncedFlag`,
  // so a paused account's per-tick `active` blip cannot flash the bar in and
  // out. Snapshotting on the table alone therefore raced that debounce and
  // sometimes captured a page with NO bar at all, which shifts every element up
  // by its ~30px height and diffs the entire image.
  //
  // Waiting on the label's settled TEXT (not merely its presence) closes all
  // three gaps at once: the store has hydrated, the debounce has elapsed, and
  // the bar is rendering its final value rather than an intermediate one.
  await expect(page.getByTestId("global-progress-label")).toHaveText(
    "Backing up - 40% (128 of 320 files)"
  );
  await snapshot(page, "backup-running.png");
});

// The pending-work queue (issue #304). Two shots: the busy panel - one running
// item with its progress bar plus one of every pending kind, so every glyph and
// subtitle is asserted - and the empty state with its next-scheduled line. The
// panel is default-collapsed, so both open it first; the badge on the collapsed
// trigger is covered by the shots above.
test("the work queue panel, busy", async ({ page, visit }) => {
  await visit("/activity", {
    commands: { get_work_queue: WORK_QUEUE_BUSY, get_sync_status: SYNC_STATUS_RUNNING },
  });
  // Wait out the global progress bar's 500ms debounce FIRST (see the
  // "a backup is running" scenario above): it is ~30px of sticky chrome that
  // shifts the whole page, so opening the panel before it settles would bake a
  // race into this baseline.
  await expect(page.getByTestId("global-progress-label")).toHaveText(
    "Backing up - 40% (128 of 320 files)"
  );
  await page.getByTestId("dropdown-trigger").click();
  // Then wait on the settled row text, not merely the panel's presence: the
  // running row's counters come from the separately-hydrated progress store.
  await expect(page.getByText("128 / 320 files")).toBeVisible();
  await snapshot(page, "work-queue-busy.png");
});

test("the work queue panel, empty", async ({ page, visit }) => {
  await visit("/activity");
  await page.getByTestId("dropdown-trigger").click();
  await expect(page.getByTestId("work-queue-empty")).toBeVisible();
  await snapshot(page, "work-queue-empty.png");
});

test("backups paused indefinitely", async ({ page, visit }) => {
  await visit("/activity", { commands: { get_pause_state: { kind: "indefinite" } } });
  await expect(page.getByText("Backups paused indefinitely")).toBeVisible();
  await snapshot(page, "paused-indefinite.png");
});

test("backups paused with a countdown", async ({ page, visit }) => {
  await visit("/activity", {
    commands: { get_pause_state: { kind: "timed", until_ms: FIXED_NOW + 30 * 60 * 1000 } },
  });
  await expect(page.getByText("Backups paused - 30m left")).toBeVisible();
  await snapshot(page, "paused-timed.png");
});

// The gate-reason banner is derived from the per-account orchestrator state, so
// it is driven by scripting `get_sync_status` into a paused state rather than
// by a dedicated command.
test("paused because the machine is on battery", async ({ page, visit }) => {
  await visit("/activity", {
    commands: {
      get_sync_status: {
        accounts: [
          { account_id: ACCOUNT_DRIVE.id, state: { state: "paused", reason: "battery" } },
          { account_id: ACCOUNT_S3.id, state: { state: "idle", last_run_at: FIXED_NOW } },
        ],
      },
    },
  });
  await expect(page.getByText("Paused - on battery power")).toBeVisible();
  await snapshot(page, "gate-battery.png");
});

test("paused because there is no internet connection", async ({ page, visit }) => {
  await visit("/activity", {
    commands: {
      get_sync_status: {
        accounts: [{ account_id: ACCOUNT_DRIVE.id, state: { state: "paused", reason: "offline" } }],
      },
    },
  });
  await expect(page.getByText("Paused - no internet connection")).toBeVisible();
  await snapshot(page, "gate-offline.png");
});

test("an update is ready to install", async ({ page, visit }) => {
  await visit("/activity", { commands: { get_pending_update_info: UPDATE_INFO } });
  await expect(page.getByText("Showing 12 of 12 events")).toBeVisible();
  await snapshot(page, "update-banner.png");
});

// `account:needs_reauth` is subscribed by AccountList, not by the shell, so it
// is only live on the Accounts tab - emitting it on /activity would notify
// nobody. This is the LIVE-event path; settings.spec.ts covers the same state
// arriving in the initial `list_accounts` payload instead.
test("an account starts needing reconnection while the tab is open", async ({ page, visit }) => {
  await visit("/accounts");
  await expect(page.getByRole("heading", { name: "Connected accounts" })).toBeVisible();
  const notified = await page.evaluate(
    () =>
      window.__drivenMock?.emit("account:needs_reauth", {
        account_id: "acct-drive-1",
        email: "ada@example.com",
      }) ?? 0
  );
  // Zero would mean nothing had subscribed and the shot would be the idle
  // shell wearing this test's name.
  expect(notified).toBeGreaterThan(0);
  await snapshot(page, "needs-reauth.png");
});
