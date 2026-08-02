<script setup lang="ts">
import AccountList from "../../components/AccountList.vue";

// Accounts settings page (SDD 2026-08-02 settings-sidebar-ia, task 4). A thin
// routed wrapper around the pre-existing AccountList - its lifecycle/IPC
// subscriptions are component-scoped and move for free. AccountList already
// renders its own page-level heading (settings.accounts.title), so this
// wrapper adds no heading of its own - it exists only to give /settings/accounts
// a dedicated page component, matching the other eight settings routes.
</script>

<template>
  <AccountList />
</template>
