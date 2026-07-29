<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted } from "vue";
import { useI18n } from "vue-i18n";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

import { onActivityNew } from "../ipc/events";
import { useFdaBannerStore } from "../stores/fdaBanner";

// The Full Disk Access banner (DESIGN s5.3.2 - macOS TCC): when macOS refuses
// Driven a read, the backup does not fail loudly, it just silently skips the
// file every cycle forever. This bar is the only place the user is told that is
// happening, and it carries the one-click fix.
//
// It owns its own `activity:new` subscription rather than reading the activity
// store, for the same reason ToastHost does: it is mounted at the app ROOT and
// never unmounted, so it cannot miss a denial that lands while the user is on
// some other route. Which rows count, and the per-file dedupe that stops a
// re-scanned denial from inflating the number every cycle, live in the store.
const { t } = useI18n();
const store = useFdaBannerStore();

// The System Settings deep link. macOS resolves the pane from the bundle id and
// the anchor after `?`; this exact string lands on Privacy & Security > Full
// Disk Access (a wrong anchor silently degrades to the Privacy & Security root,
// so it is verified verbatim in the test rather than assembled at runtime).
const FULL_DISK_ACCESS_URL =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

// Bound, not a template literal, so the i18n no-raw-text rule is satisfied and
// the plural choice (file vs files) is passed the way the rest of the app does.
const body = computed<string>(() =>
  t("fdaBanner.body", { count: store.deniedCount }, store.deniedCount)
);

async function onOpenSettings(): Promise<void> {
  try {
    await openUrl(FULL_DISK_ACCESS_URL);
  } catch (e) {
    // Opening System Settings is a convenience, never a failure path worth
    // interrupting the user for: the banner text already names the pane, so a
    // rejected open (no opener, sandbox refusal) just logs. Swallowing it here
    // also keeps a rejected promise from surfacing as an unhandled rejection.
    console.error("failed to open the Full Disk Access pane", e);
  }
}

let unlisteners: UnlistenFn[] = [];
let disposed = false;

onMounted(async () => {
  const registered: UnlistenFn[] = [];
  try {
    registered.push(await onActivityNew((entry) => store.noteDenial(entry)));
  } catch (e) {
    // Same policy as the toast subscriptions: a banner is never a reason to
    // break boot. Tear down whatever did register so a half-wired subscription
    // cannot leak, and log.
    for (const un of registered) un();
    console.error("FDA banner subscribe failed", e);
    return;
  }
  // Unmount may have raced ahead while we awaited `listen`; honor it rather than
  // leaving a listener registered against a dead component.
  if (disposed) {
    for (const un of registered) un();
    return;
  }
  unlisteners = registered;
});

onBeforeUnmount(() => {
  disposed = true;
  for (const un of unlisteners) un();
  unlisteners = [];
});
</script>

<template>
  <div
    v-if="store.visible"
    class="mx-6 my-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800/60 dark:bg-amber-950/40"
    role="status"
    data-testid="fda-banner"
  >
    <h4 class="font-semibold text-amber-800 dark:text-amber-300">{{ t("fdaBanner.title") }}</h4>
    <p class="mt-1 text-amber-700 dark:text-amber-200/80">{{ body }}</p>
    <p class="mt-1 text-amber-700 dark:text-amber-200/80" data-testid="fda-banner-unsigned-note">
      {{ t("fdaBanner.unsignedNote") }}
    </p>
    <div class="mt-2 flex flex-wrap items-center gap-2">
      <button
        type="button"
        class="rounded-sm border border-amber-500 px-2 py-0.5 font-medium text-amber-900 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 dark:border-amber-400/70 dark:text-amber-100 dark:hover:bg-amber-900/50"
        data-testid="fda-banner-open"
        @click="onOpenSettings"
      >
        {{ t("fdaBanner.openButton") }}
      </button>
      <button
        type="button"
        class="rounded-sm px-2 py-0.5 font-medium text-amber-800 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 dark:text-amber-200 dark:hover:bg-amber-900/50"
        data-testid="fda-banner-dismiss"
        @click="store.dismiss()"
      >
        {{ t("common.close") }}
      </button>
    </div>
  </div>
</template>
