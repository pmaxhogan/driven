<script setup lang="ts">
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { cardCls, ensureSettingsLoaded } from "./shared";

// Startup card (SDD 2026-08-02 settings-sidebar-ia, task 7 fix). Extracted
// verbatim out of PlatformPage.vue (markup + setAutoStartOnLogin) so it can be
// rendered by TWO parents: PlatformPage (its original home, macOS/Windows)
// and GeneralPage (Linux, where autoStartOnLogin is otherwise unreachable -
// SettingsNav hides the Platform nav item, and search skips hidden items,
// whenever both `settings.macos` and `settings.windows` are null). Each
// parent decides WHETHER to render this card; the card itself has no
// platform-visibility logic - it's a GLOBAL setting the backend supports on
// every OS (lib.rs:388-391 registers a .desktop launcher on Linux).
//
// Calls ensureSettingsLoaded() itself (rather than relying on the parent to
// have loaded settings first) so it renders correctly whether mounted
// standalone or nested - shared.ts's `!settings.loading` guard makes a
// second call here a no-op when the parent page already triggered one.
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch } = useSettingsForm();

ensureSettingsLoaded();

// Issue #58: launch Driven at login (default ON). Patches the persisted
// preference; the backend registers/unregisters the real OS startup entry.
async function setAutoStartOnLogin(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { autoStartOnLogin: checked } });
}
</script>

<template>
  <section
    v-if="settings.settings"
    class="space-y-3"
    :class="cardCls"
    data-testid="startup-setting"
  >
    <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
      {{ t("settings.rules.sections.startup") }}
    </h3>
    <label class="flex items-center gap-2">
      <input
        type="checkbox"
        class="accent-teal-600"
        data-testid="autostart-toggle"
        :checked="settings.settings.global.autoStartOnLogin"
        @change="setAutoStartOnLogin"
      />
      {{ t("settings.rules.autoStartOnLoginLabel") }}
    </label>
    <p class="text-xs text-zinc-500 dark:text-zinc-400">
      {{ t("settings.rules.autoStartOnLoginNote") }}
    </p>
  </section>
</template>
