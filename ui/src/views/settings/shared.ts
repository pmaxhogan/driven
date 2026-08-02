import { onMounted } from "vue";

import { useSettingsStore } from "../../stores/settings";

// Shared design-system class strings (DRIVEN UI design system), extracted
// verbatim from Settings.vue (SDD 2026-08-02 settings-sidebar-ia, task 2) so
// every Rules-tab page can reach the same native-control styling without
// duplicating the string. Native controls MUST carry explicit light/dark
// surface + text colors so they stay readable on a dark-theme OS; teal is the
// accent for focus rings.
export const inputCls =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-hidden focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
export const cardCls =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-xs dark:border-zinc-800 dark:bg-zinc-900";

// Each Rules-tab page is now a self-contained SFC with no props - it reads
// `useSettingsStore` directly rather than waiting on a parent (Settings.vue)
// to have loaded the snapshot first. Previously a single `watch(active, ...,
// { immediate: true })` in Settings.vue triggered `settings.refresh()` once
// when the Rules tab became active; with the form split across seven
// independently-mountable pages, every page that reads `settings.settings`
// calls this on mount instead, so a page is never stuck showing its loading
// state just because it happened to render before some sibling page's
// trigger fired (or, in a page-only test mount, because no sibling exists at
// all).
//
// The `!settings.loading` check matters: `settings.refresh()` sets
// `loading.value = true` SYNCHRONOUSLY before its first `await`, and Vue
// flushes sibling `onMounted` hooks together in one synchronous pass with no
// microtask gap between them. So when Settings.vue stacks all seven pages at
// once, GeneralPage's onMounted sets `loading` true and starts the fetch;
// SchedulePowerPage's onMounted runs next (still synchronously) and sees
// `loading` already true, so it skips its own call. Without this guard, all
// seven pages would fire `get_settings` concurrently AND race to write
// `settings.errorCode` - the store has no in-flight de-dupe of its own, so
// the last response to land would silently overwrite the others' result.
export function ensureSettingsLoaded(): void {
  const settings = useSettingsStore();
  onMounted(() => {
    if (settings.settings === null && !settings.loading) {
      void settings.refresh();
    }
  });
}
