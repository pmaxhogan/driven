# Visual regression suite

Playwright screenshot tests for every main UI surface, run against the **real**
Vue app with the Tauri IPC layer replaced by a scripted mock. No Rust, no native
webview, no Google account, no network - so an agent or a CI job can catch a
visual regression without building or installing the desktop app.

```sh
pnpm -C ui run test:visual          # check against the committed baselines
just visual                         # the same thing, from the repo root
just visual-update                  # regenerate the linux baselines (Docker)
```

## How it works

The whole app reaches the backend through one hole in the wall:
`window.__TAURI_INTERNALS__.invoke`. `@tauri-apps/api`'s `invoke` forwards
straight to it, `listen` is itself an `invoke("plugin:event|listen", ...)`, and
the plugin wrappers the app uses (`openUrl`, `getVersion`) are invokes too.

[`ui/test-support/mock-backend.ts`](../test-support/mock-backend.ts) fills that
hole from a declarative table, with a realistic default for **every** command in
`tauri::generate_handler![...]`. A spec overrides only the commands its surface
depends on:

```ts
await visit("/activity", {
  commands: {
    query_activity: ACTIVITY_PAGE_EMPTY, // a plain value resolves
    get_settings: mockError("state.db_locked"), // reject with a SPEC s24 code
    list_scrub_runs: mockPending(), // never settle -> loading state
  },
});
await snapshot(page, "empty.png");
```

Rust -> webview events are driven through the same mock:

```ts
const notified = await page.evaluate(
  (status) => window.__drivenMock?.emit("restore:progress", status) ?? 0,
  RESTORE_JOB_RUNNING
);
expect(notified).toBeGreaterThan(0); // 0 means nothing had subscribed yet
```

A command with **no** scripted response rejects loudly rather than resolving
`undefined`. That is deliberate: a silently-undefined answer renders a
plausible-but-wrong screenshot, which is worse than a failing test. So when a new
`#[tauri::command]` is added to the app, add a default to `defaultCommands()`.

The same module works under vitest - `installScenario({...})` in a `beforeEach`
gives a whole working backend instead of a handful of hand-stubbed commands. See
`src/__tests__/mock-backend.test.ts`. Existing vitest suites mock
`@tauri-apps/api/core` at the module level and were deliberately left alone.

## Determinism

A screenshot has to be a pure function of the source, or the suite becomes noise
everyone learns to ignore. What is pinned, and why:

| Pinned                               | Otherwise                                            |
| ------------------------------------ | ---------------------------------------------------- |
| Viewport 1280x800, scale 1           | Layout differs per host display                      |
| `timezoneId: "UTC"`, `locale: en-US` | `Intl` renders different dates and number separators |
| Clock at `FIXED_NOW` (2026-03-15Z)   | "5 minutes ago" changes every run                    |
| Fixture timestamps derived from it   | Same                                                 |
| App version faked as `9.9.9`         | release-please bumps the real one every release      |
| Linux user agent                     | `src/platform.ts` branches About and Rules on the UA |
| `reducedMotion`, `animations: off`   | Skeletons and transitions land mid-frame             |
| `screenshot.css` hides scrollbars    | Classic vs overlay scrollbars shift the layout       |

The two platform-specific surfaces are covered by `test.use({ userAgent })` on a
describe block (see `settings.spec.ts`) rather than by extra Playwright
projects - the baseline count is already doubled by the two colour schemes.

Colour schemes are two projects, `dark` and `light`. The app has no theme toggle
and no class-based dark variant: Tailwind v4's default `dark:` variant is
`prefers-color-scheme`, which is what Playwright's `colorScheme` drives, so the
projects really do exercise both themes of the shipped CSS.

## Baselines are linux, and CI is the gate

Text rasterization differs per OS, so a baseline is only meaningful on the
platform that produced it. **CI runs ubuntu, so the committed baselines are
linux** and live in `__screenshots__/linux/`. Everything else under
`__screenshots__/` is gitignored.

Regenerate them in the official Playwright container, never from a host:

```sh
just visual-update
```

which is:

```sh
docker run --rm \
  -v "$PWD":/work -v /work/ui/node_modules -v /work/.pnpm-store -w /work/ui \
  mcr.microsoft.com/playwright:v1.62.1-noble \
  sh -c "corepack enable && pnpm install --frozen-lockfile \
         && pnpm exec playwright test --update-snapshots"
```

Both anonymous volumes are load-bearing. `/work/ui/node_modules` shadows the
host's tree so the container installs its own - `esbuild`, `rollup` and
`@tailwindcss/oxide` ship platform-native binaries, and a macOS tree hard-fails
inside linux. `/work/.pnpm-store` catches pnpm's store: it cannot hardlink from
the container HOME onto a volume, so it falls back to a store beside the
project, which on a bind mount would dump ~250 MB into your working tree.

The image tag must match the resolved `@playwright/test` version in
`ui/package.json` exactly.

These container-generated baselines were verified to pass on a bare
`ubuntu:24.04` running `playwright install --with-deps chromium` - the same
shape as the GitHub `ubuntu-latest` runner - so the container and CI agree.

**Running locally on macOS or Windows is advisory.** The first run writes its own
baselines under `__screenshots__/darwin/` (or `windows/`) and reports each as
"A snapshot doesn't exist ..., writing actual" - which Playwright counts as a
failure. That is expected on a first run; run it again and it passes. Those trees
are gitignored: they are useful for eyeballing your own change, but linux is the
only truth, and a change that looks right locally still has to be regenerated
through Docker before it is committed.

## Adding a spec

1. Pick the route and the scenario. Remember the router's first-run guard sends
   `/` and `/activity` to `/setup` when `list_accounts` is empty - an "empty
   Activity" scenario therefore still needs at least one account. Deep links to
   any other route are always honoured.
2. `await visit(path, scenario)`.
3. Wait on something that proves the data landed (a row count, a testid) - not
   just the page heading, which renders before the first IPC resolves.
4. `await snapshot(page, "name.png")`.
5. `just visual-update`, eyeball the new PNGs, commit them with the spec.
