<script setup lang="ts">
import SourceTable from "../../components/SourceTable.vue";

// Sources settings page (SDD 2026-08-02 settings-sidebar-ia, task 4). A thin
// routed wrapper around the pre-existing SourceTable - its lifecycle/IPC
// subscriptions are component-scoped and move for free. SourceTable already
// renders its own page-level heading (settings.sources.title), so this
// wrapper adds no heading of its own - it exists only to give /settings/sources
// a dedicated page component, matching the other eight settings routes.
</script>

<template>
  <SourceTable />
</template>
