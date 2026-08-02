// Shared Playwright fixtures for the visual suite.
//
// `visit(path, scenario)` is the whole setup API: it pins the clock, installs
// the scripted backend as an init script (so the mock is in place before any
// app code runs - the router's first-run guard calls `list_accounts` during the
// very first navigation), and navigates.
//
// `snapshot(page, name)` is the whole assertion API.

import { test as base, expect, type Page } from "@playwright/test";

import { FIXED_NOW } from "../test-support/fixtures";
import {
  installMockBackend,
  resolveScenario,
  type MockScenario,
} from "../test-support/mock-backend";

interface VisualFixtures {
  /** Navigate to `path` with the scripted backend installed. */
  visit: (path: string, scenario?: MockScenario) => Promise<void>;
}

export const test = base.extend<VisualFixtures>({
  visit: async ({ page }, use) => {
    await use(async (path: string, scenario: MockScenario = {}) => {
      // Pin Date WITHOUT freezing timers: several components hold a ticking
      // `now` ref (StatusBanner, the pause store) and rely on their interval
      // still firing, so `install()` would hang them.
      await page.clock.setFixedTime(new Date(FIXED_NOW));
      await page.addInitScript(installMockBackend, resolveScenario(scenario));
      await page.goto(path);
    });
  },
});

/**
 * Assert the WHOLE page against its committed baseline.
 *
 * Full page rather than viewport: Settings, Activity and Restore are all much
 * taller than the 1280x800 viewport, and a viewport-only capture would silently
 * stop asserting anything below the fold - which is where most of the surface
 * lives. Chromium captures beyond the viewport in one pass, so sticky chrome is
 * rendered once rather than repeated down the image.
 */
export async function snapshot(page: Page, name: string): Promise<void> {
  await expect(page).toHaveScreenshot(name, { fullPage: true });
}

export { expect };
export { FIXED_NOW };
