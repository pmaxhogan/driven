import { defineConfig } from "@playwright/test";

// Visual-regression suite (ui/e2e-visual/README.md).
//
// The app under test is the REAL Vue app served by `vite preview`, with the
// Tauri IPC layer replaced by the scripted mock in ui/test-support. No Rust, no
// native webview, no network.
//
// Everything below exists to make a screenshot a pure function of the source:
// a fixed viewport and scale, a pinned timezone and locale (the Activity and
// Restore views format timestamps through `Intl`), reduced motion (which also
// switches off every `motion-safe:animate-*` skeleton), and disabled
// animations at capture time.
//
// Baselines are LINUX and are generated in the official Playwright container -
// see the README. A run on any other platform writes its own baselines under a
// gitignored `__screenshots__/<platform>/` tree and is advisory only.

const PORT = 4173;

export default defineConfig({
  testDir: "./e2e-visual",

  // Screenshots are the assertion, so a "flaky" pass is a lie: never retry.
  retries: 0,
  forbidOnly: !!process.env.CI,
  fullyParallel: true,
  workers: process.env.CI ? 2 : undefined,
  reporter: process.env.CI ? [["github"], ["html", { open: "never" }]] : [["list"]],

  // Platform first so the whole gitignored non-linux tree is one directory.
  snapshotPathTemplate: "{testDir}/__screenshots__/{platform}/{projectName}/{testFileName}/{arg}{ext}",

  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    viewport: { width: 1280, height: 800 },
    deviceScaleFactor: 1,
    reducedMotion: "reduce",
    timezoneId: "UTC",
    locale: "en-US",
    // Pinned rather than inherited from the browser build. `src/platform.ts`
    // branches the About and Rules surfaces on the user agent, so an unpinned
    // UA would render a macOS variant locally and a Linux one in the container
    // from the same source. Specs that WANT the macOS or Windows variant say so
    // with `test.use({ userAgent })`.
    userAgent:
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) " +
      "Chrome/140.0.0.0 Safari/537.36",
    trace: "retain-on-failure",
  },

  // One project per colour scheme. The app has no theme toggle and no
  // class-based dark variant: Tailwind v4's default `dark:` variant is
  // `prefers-color-scheme`, which is exactly what `colorScheme` drives - so
  // these two projects really do exercise both themes of the shipped CSS.
  projects: [
    { name: "dark", use: { colorScheme: "dark" } },
    { name: "light", use: { colorScheme: "light" } },
  ],

  expect: {
    toHaveScreenshot: {
      // `fullPage` is deliberately NOT here - it is not a config-level option
      // (only `threshold`, `maxDiff*`, `animations`, `caret`, `scale`,
      // `stylePath`, `pathTemplate` and `timeout` are). The `snapshot()` helper
      // in e2e-visual/fixtures.ts passes it per call instead.
      stylePath: "./e2e-visual/screenshot.css",
      animations: "disabled",
      caret: "hide",
      scale: "css",
      // ABSOLUTE, not a ratio. These are full-page captures and Settings runs
      // past 2500px tall, so `maxDiffPixelRatio: 0.01` would have allowed
      // ~32k differing pixels there - a 180x180 block, big enough for a whole
      // control to vanish unnoticed. An absolute budget does not grow with page
      // height.
      //
      // 200px is small because it is not the thing absorbing antialiasing drift:
      // `threshold` (per-pixel YIQ distance, default 0.2) already does that
      // before a pixel is ever counted as differing. Verified empirically - the
      // container-generated baselines pass at this budget on a bare ubuntu:24.04
      // with `playwright install --with-deps`, which is the CI runner's shape.
      // If one spec genuinely needs slack, override it at that call site rather
      // than loosening this.
      maxDiffPixels: 200,
    },
  },

  webServer: {
    // A production build, not the dev server: no HMR client, no dev overlay,
    // and the same asset pipeline the shipped app uses. `vite build` rather
    // than `pnpm build` deliberately skips the `vue-tsc` pass, which the lint
    // and build CI jobs already own.
    // `--host 127.0.0.1` is load-bearing: left to itself `vite preview` binds
    // the `localhost` alias, which resolves to `::1` first on some hosts, and
    // the readiness poll below (an IPv4 literal) then never connects.
    command:
      `pnpm exec vite build && ` +
      `pnpm exec vite preview --host 127.0.0.1 --port ${PORT} --strictPort`,
    url: `http://127.0.0.1:${PORT}`,
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
