import { defineConfig, mergeConfig } from "vitest/config";
import viteConfig from "./vite.config";

// Coverage / test config lives HERE, not in vite.config.ts, on purpose.
// vue-tsc typechecks vite.config.ts (it is in tsconfig "include") but NOT this
// file, so putting the `test` block here keeps vitest's own vite types from
// clashing with the app's vite-typed plugins - vitest 2.x still pins
// `vite: ^5.0.0 || ^6.0.0` (vitest 4 is the first line that declares vite 8
// support, but its AST-aware v8 coverage remapping reports substantially
// LOWER numbers - not a real regression, just a stricter measurement - which
// the CI coverage gate can't clear without a baseline reset, so the vitest
// bump is deliberately deferred; don't "helpfully" bump it), while the app
// itself runs vite 8. The pnpm tree resolves vite 5, 6, AND 8 side by side as
// a result, and importing vitest/config's defineConfig into a typechecked
// file makes that clash a hard `vue-tsc` error.
//
// vitest auto-loads vitest.config.* ahead of vite.config.*; mergeConfig folds
// in the vue / i18n plugins + alias so component tests keep working. The
// `// @vitest-environment jsdom` per-file docblocks still select jsdom where a
// test needs the DOM; the default stays node.
export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      coverage: {
        provider: "v8",
        // `json-summary` feeds the CI coverage gate (.total.lines.pct); `lcov`
        // is for local HTML drilldown; `text` prints the table in the run log.
        reporter: ["text", "json-summary", "lcov"],
        reportsDirectory: "./coverage",
        // Count the shipped app code. `all: true` so an untested NEW file lands
        // as 0%-covered and drags the total DOWN, which is what the coverage
        // gate (CONTRIBUTING.md) needs to catch a regression.
        all: true,
        include: ["src/**/*.{ts,vue}"],
        exclude: [
          "src/**/*.d.ts",
          "src/main.ts",
          "src/locales/**",
          "src/**/__tests__/**",
        ],
      },
    },
  }),
);
