# `ui/`

The Vue 3 + TypeScript frontend rendered in the Tauri webview. It is a view layer
only: all state changes go through IPC to `src-tauri`, and no backup logic lives
here. Visual conventions are in `ui/DESIGN_SPEC.md`.

- `src/ipc/` - the typed boundary: `commands.ts` (invoke wrappers), `events.ts`
  (backend event subscriptions), `types.ts` (mirrors `src-tauri/src/commands/dtos.rs`),
  `errors.ts`
- `src/stores/` - Pinia stores, one per domain (`sources`, `settings`, `activity`,
  `progress`, `restore`, `toasts`, `updater`, ...); components read these, not IPC
- `src/views/` + `src/router.ts` - the top-level screens (`SetupWizard`, `Activity`,
  `Restore`, `Settings`, `About`)
- `src/components/` - shared widgets (wizards, stat tiles, exclusion tree, toasts)
- `src/locales/` + `src/i18n.ts` - message catalogs; every user-visible string is a
  key and ESLint fails the build on a missing or raw one

```sh
pnpm install
pnpm dev            # vite alone on :5173; native `invoke` calls will fail
pnpm test:unit      # vitest
pnpm lint           # eslint (incl. the i18n key rules)
```

Run the real app with `just dev` from the repo root instead when you need the
backend. A new `.vue` file needs a mount test - the `coverage` CI gate compares
against `main` and a store-only test will not clear it.
