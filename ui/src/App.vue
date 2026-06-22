<script setup lang="ts">
import { useI18n } from "vue-i18n";

const { t } = useI18n();
</script>

<template>
  <main class="min-h-screen flex items-center justify-center p-8">
    <section class="max-w-xl text-center space-y-4">
      <h1 class="text-3xl font-semibold">
        {{ t("app.welcome") }}
      </h1>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("app.tagline") }}
      </p>
      <p class="text-xs text-zinc-500">
        {{ t("app.boot.bootstrap") }}
      </p>
    </section>
  </main>
</template>
