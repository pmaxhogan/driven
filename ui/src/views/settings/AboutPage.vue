<script setup lang="ts">
import About from "../About.vue";

// About settings page (SDD 2026-08-02 settings-sidebar-ia, task 4). A thin
// routed wrapper around the pre-existing About view - its lifecycle/IPC
// subscriptions are component-scoped and move for free. About already renders
// its own page-level heading (about.title), so this wrapper adds no heading of
// its own - it exists only to give /settings/about a dedicated page component,
// matching the other eight settings routes.
</script>

<template>
  <About />
</template>
