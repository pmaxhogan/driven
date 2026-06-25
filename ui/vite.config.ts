/// <reference types="vitest/config" />
import { defineConfig } from "vitest/config";
import vue from "@vitejs/plugin-vue";
import VueI18nPlugin from "@intlify/unplugin-vue-i18n/vite";
import path from "node:path";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [
    vue(),
    VueI18nPlugin({
      include: [path.resolve(__dirname, "./src/locales/**")],
      strictMessage: false,
      runtimeOnly: false,
    }),
  ],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  clearScreen: false,
  // Vitest config. Per-file `// @vitest-environment jsdom` docblocks still
  // select jsdom where a test needs the DOM; the default stays node.
  test: {
    coverage: {
      provider: "v8",
      // `json-summary` feeds the CI coverage-gate (.total.lines.pct); `lcov`
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
  server: {
    port: 5173,
    strictPort: true,
    host: host ?? false,
    hmr: host
      ? { protocol: "ws", host, port: 5174 }
      : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
