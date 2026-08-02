import { mockError } from "../test-support/mock-backend";
import { expect, snapshot, test } from "./fixtures";

// The five-step first-run wizard (DESIGN s8.5). Step 2 renders a DIFFERENT
// credential form per destination, which is the wizard's biggest visual branch,
// so each variant gets its own shot.
//
// `/setup` is reached by deep link rather than through the first-run guard: the
// guard only diverts the default landing, and going straight there keeps these
// specs independent of how many accounts the scenario reports.

async function chooseBackendAndAdvance(
  page: import("@playwright/test").Page,
  backendId: string
): Promise<void> {
  await page.getByTestId(`backend-option-${backendId}`).getByRole("radio").check();
  await page.getByRole("button", { name: "Next" }).click();
}

test("step 1 - welcome and destination picker", async ({ page, visit }) => {
  await visit("/setup");
  await expect(page.getByTestId("backend-picker")).toBeVisible();
  await snapshot(page, "step1-welcome.png");
});

test("step 1 - no destinations available in this build", async ({ page, visit }) => {
  await visit("/setup", { commands: { list_backends: [] } });
  await expect(page.getByTestId("backend-picker-empty")).toBeVisible();
  await snapshot(page, "step1-no-backends.png");
});

test("step 2 - Google Drive OAuth walkthrough", async ({ page, visit }) => {
  await visit("/setup");
  await expect(page.getByTestId("backend-picker")).toBeVisible();
  await chooseBackendAndAdvance(page, "google_drive");
  await expect(page.getByRole("button", { name: "Sign in with Google" })).toBeVisible();
  await snapshot(page, "step2-google.png");
});

test("step 2 - S3 access key form", async ({ page, visit }) => {
  await visit("/setup");
  await expect(page.getByTestId("backend-picker")).toBeVisible();
  await chooseBackendAndAdvance(page, "s3");
  await expect(page.getByTestId("s3-credentials-form")).toBeVisible();
  await snapshot(page, "step2-s3.png");
});

test("step 2 - local folder form", async ({ page, visit }) => {
  await visit("/setup");
  await expect(page.getByTestId("backend-picker")).toBeVisible();
  await chooseBackendAndAdvance(page, "local_folder");
  await expect(page.getByTestId("local-folder-form")).toBeVisible();
  await snapshot(page, "step2-local-folder.png");
});

test("step 2 - the destination rejects the credentials", async ({ page, visit }) => {
  await visit("/setup", {
    commands: { create_local_folder_account: mockError("local.permission_denied") },
  });
  await expect(page.getByTestId("backend-picker")).toBeVisible();
  await chooseBackendAndAdvance(page, "local_folder");
  await page.getByTestId("local-folder-choose").click();
  await page.getByTestId("local-folder-connect").click();
  // Two alerts render: the form's own inline message and the wizard-level one
  // below the card. Both are wanted in the shot; wait on the first.
  await expect(page.getByRole("alert").first()).toBeVisible();
  await snapshot(page, "step2-error.png");
});

// Steps 3 to 5 are reached through the LOCAL FOLDER destination: it is the only
// one whose credential step completes without an OAuth consent round trip, so
// the walk stays a handful of deterministic clicks.
test.describe("later steps, via a local-folder account", () => {
  async function reachSourceStep(page: import("@playwright/test").Page): Promise<void> {
    await expect(page.getByTestId("backend-picker")).toBeVisible();
    await chooseBackendAndAdvance(page, "local_folder");
    await page.getByTestId("local-folder-choose").click();
    await page.getByTestId("local-folder-connect").click();
    await expect(page.getByTestId("wizard-choose-folder")).toBeVisible();
  }

  test("step 3 - choose the first backup source", async ({ page, visit }) => {
    await visit("/setup");
    await reachSourceStep(page);
    await snapshot(page, "step3-source.png");
  });

  test("step 4 - encryption opt-in", async ({ page, visit }) => {
    await visit("/setup");
    await reachSourceStep(page);
    await page.getByTestId("wizard-choose-folder").click();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByRole("button", { name: "Next" })).toBeEnabled();
    await snapshot(page, "step4-encryption.png");
  });

  test("step 5 - confirm and start", async ({ page, visit }) => {
    await visit("/setup");
    await reachSourceStep(page);
    await page.getByTestId("wizard-choose-folder").click();
    await page.getByRole("button", { name: "Next" }).click();
    await page.getByRole("button", { name: "Next" }).click();
    await expect(page.getByRole("button", { name: "Start backup" })).toBeVisible();
    await snapshot(page, "step5-confirm.png");
  });
});
