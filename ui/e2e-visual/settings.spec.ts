import { APFS_HELPER_STATUS, SETTINGS, VSS_HELPER_STATUS } from "../test-support/fixtures";
import { mockError, mockPending } from "../test-support/mock-backend";
import { expect, snapshot, test } from "./fixtures";

// The settings surface (v2.8.0 sidebar IA, PR #240): one /settings shell with
// a sidebar and nine nested pages. Each page has its own route, so the specs
// navigate straight to it; the sidebar itself is captured in every shot.

test.describe("pages", () => {
  test("accounts", async ({ page, visit }) => {
    await visit("/settings/accounts");
    await expect(page.getByRole("heading", { name: "Connected accounts" })).toBeVisible();
    await snapshot(page, "accounts.png");
  });

  test("sources", async ({ page, visit }) => {
    await visit("/settings/sources");
    await expect(page.getByRole("heading", { name: "Backup sources" })).toBeVisible();
    await snapshot(page, "sources.png");
  });

  test("general", async ({ page, visit }) => {
    await visit("/settings/general");
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "general.png");
  });

  test("schedule and power", async ({ page, visit }) => {
    await visit("/settings/schedule-power");
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "schedule-power.png");
  });

  test("performance", async ({ page, visit }) => {
    await visit("/settings/performance");
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "performance.png");
  });

  test("network", async ({ page, visit }) => {
    await visit("/settings/network");
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "network.png");
  });

  test("privacy", async ({ page, visit }) => {
    await visit("/settings/privacy");
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "privacy.png");
  });

  test("advanced", async ({ page, visit }) => {
    await visit("/settings/advanced");
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "advanced.png");
  });

  test("about", async ({ page, visit }) => {
    await visit("/settings/about");
    await expect(page.getByRole("heading", { name: "About Driven" })).toBeVisible();
    await snapshot(page, "about.png");
  });

  test("legacy routes redirect into the new IA", async ({ page, visit }) => {
    // The pre-2.8.0 tab routes must keep working (deep links, muscle memory).
    await visit("/rules");
    await expect(page).toHaveURL(/\/settings\/general$/);
    // One redirect proves the mapping table is wired; the other two are
    // asserted URL-only to keep the baseline count flat.
    await visit("/accounts");
    await expect(page).toHaveURL(/\/settings\/accounts$/);
    await visit("/sources");
    await expect(page).toHaveURL(/\/settings\/sources$/);
  });
});

test.describe("empty and degraded states", () => {
  test("no accounts", async ({ page, visit }) => {
    // A deep link is honoured by the first-run guard even with zero accounts,
    // so this really does render the empty Accounts page rather than the wizard.
    await visit("/settings/accounts", { commands: { list_accounts: [], list_sources: [] } });
    await expect(page.getByText("No accounts connected yet")).toBeVisible();
    await snapshot(page, "accounts-empty.png");
  });

  test("no sources", async ({ page, visit }) => {
    await visit("/settings/sources", { commands: { list_sources: [] } });
    await expect(page.getByText("No backup sources yet")).toBeVisible();
    await snapshot(page, "sources-empty.png");
  });

  test("an account needs reconnecting", async ({ page, visit }) => {
    await visit("/settings/accounts", {
      commands: {
        list_accounts: [
          {
            id: "acct-drive-2",
            email: "grace@example.com",
            displayName: null,
            state: "needs_reauth",
            encryptionEnabled: false,
            createdAt: 0,
            lastSyncedAt: null,
            backendKind: "google_drive",
          },
        ],
      },
    });
    await expect(page.getByText("Reconnect required")).toBeVisible();
    await snapshot(page, "accounts-needs-reauth.png");
  });

  test("settings fail to load", async ({ page, visit }) => {
    await visit("/settings/general", {
      commands: { get_settings: mockError("state.db_corrupt") },
    });
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "general-error.png");
  });

  test("settings still loading", async ({ page, visit }) => {
    await visit("/settings/general", { commands: { get_settings: mockPending() } });
    // Wait on the loading affordance itself, not the heading - the heading
    // renders before any IPC resolves, so it would pass even if the loading
    // state had silently stopped rendering.
    await expect(page.getByText("Loading...").first()).toBeVisible();
    await snapshot(page, "general-loading.png");
  });

  test("an update is available", async ({ page, visit }) => {
    await visit("/settings/about", {
      commands: {
        check_for_update: {
          version: "10.0.0",
          notes: "- Faster incremental scans",
          publishedAt: "2026-03-14T09:30:00Z",
          channel: "stable",
        },
        get_pending_update_info: {
          version: "10.0.0",
          notes: "- Faster incremental scans",
          publishedAt: "2026-03-14T09:30:00Z",
          channel: "stable",
        },
      },
    });
    // The version appears twice - the dismissable banner and the check result -
    // so target the banner rather than the ambiguous text.
    await expect(page.getByTestId("update-banner")).toBeVisible();
    await snapshot(page, "about-update-available.png");
  });
});

// `src/platform.ts` branches on the user agent, and `get_settings` reports a
// non-null `windows` / `macos` block only on that OS. Both have to be faked
// together or the page renders a state the real app never produces. These are
// per-describe `test.use` overrides rather than extra Playwright projects: the
// baseline count is already doubled by the two colour schemes, and only these
// surfaces differ by platform.

test.describe("macOS variant", () => {
  test.use({
    userAgent:
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 " +
      "(KHTML, like Gecko) Version/17.0 Safari/605.1.15",
  });

  test("platform page shows the APFS snapshot controls", async ({ page, visit }) => {
    await visit("/settings/platform", {
      commands: {
        get_settings: {
          ...SETTINGS,
          macos: {
            apfsSnapshot: true,
            menuBar: {
              showUploadSpeed: true,
              showPercent: true,
              showFiles: false,
              showEta: false,
              idle: "lastBackupAge",
            },
          },
        },
        get_apfs_helper_status: { ...APFS_HELPER_STATUS, supported: true, helperEnabled: true },
      },
    });
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "platform-macos.png");
  });

  test("about page offers a manual download instead of an in-app update", async ({
    page,
    visit,
  }) => {
    await visit("/settings/about");
    await expect(page.getByRole("heading", { name: "About Driven" })).toBeVisible();
    await snapshot(page, "about-macos.png");
  });
});

test.describe("Windows variant", () => {
  test.use({
    userAgent:
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
      "(KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
  });

  test("platform page shows the locked-file backup controls", async ({ page, visit }) => {
    await visit("/settings/platform", {
      commands: {
        get_settings: { ...SETTINGS, windows: { vssMode: "auto", vssHelper: true } },
        get_vss_helper_status: {
          ...VSS_HELPER_STATUS,
          supported: true,
          helperEnabled: true,
          helperLaunchable: true,
        },
      },
    });
    await expect(page.getByRole("heading").first()).toBeVisible();
    await snapshot(page, "platform-windows.png");
  });

  test("sources page shows the cloud-only placeholder policy", async ({ page, visit }) => {
    await visit("/settings/sources");
    await expect(page.getByRole("heading", { name: "Backup sources" })).toBeVisible();
    await snapshot(page, "sources-windows.png");
  });
});
