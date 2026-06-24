<script setup lang="ts">
import { useI18n } from "vue-i18n";

// M8 placeholder (per the M6 scope decision): the restore browser is built in a
// later milestone. M6 renders a simple "coming later" shell via t() so the SPEC
// s25 /restore + /restore/:sourceId routes resolve.
defineProps<{ sourceId?: string }>();

const { t } = useI18n();
</script>

<template>
  <section class="space-y-2">
    <h1 class="text-2xl font-semibold">
      {{ t("restore.title") }}
    </h1>
    <p class="text-zinc-600 dark:text-zinc-400">
      {{ t("restore.placeholder") }}
    </p>
  </section>
</template>
