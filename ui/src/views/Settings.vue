<script setup lang="ts">
import { useI18n } from "vue-i18n";

import SettingsNav from "../components/SettingsNav.vue";

// Settings shell (SPEC s25 /settings; SDD 2026-08-02 settings-sidebar-ia, task
// 3). The old four-tab strip (Accounts / Sources / Rules / About) and its `tab`
// prop are gone: this view is now just a sidebar (SettingsNav) plus a routed
// content pane. Every settings surface - Accounts, Sources, the seven Rules
// pages, About - is its own child route under /settings (router.ts), rendered
// through <RouterView>, so this shell holds no page state of its own. The
// pre-sidebar flat paths (/accounts, /sources, /rules, /about) still resolve -
// they redirect to their nested equivalent - so every existing deep link keeps
// working.
const { t } = useI18n();
</script>

<template>
  <section class="flex gap-6">
    <SettingsNav />
    <div class="min-w-0 flex-1 space-y-4">
      <h1 class="text-2xl font-semibold">
        {{ t("settings.title") }}
      </h1>
      <RouterView />
    </div>
  </section>
</template>
