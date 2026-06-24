<script setup lang="ts">
import { onMounted } from "vue";
import { useI18n } from "vue-i18n";

import AddSourceWizard from "./AddSourceWizard.vue";
import { useSourcesStore } from "../stores/sources";

// Sources settings tab body (SPEC s11.2; DESIGN s8.2). M6 shell: a table of
// sources with the per-row affordances the design calls for (enabled toggle,
// edit exclusions, run now, remove). The sources implementer wires the row
// actions + the add-source wizard launch; the table + store refresh are live.
const { t } = useI18n();
const sources = useSourcesStore();

onMounted(() => {
  void sources.refresh();
});
</script>

<template>
  <div class="space-y-3">
    <div class="flex items-center justify-between">
      <h2 class="text-lg font-medium">
        {{ t("settings.sources.title") }}
      </h2>
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
      >
        {{ t("settings.sources.addButton") }}
      </button>
    </div>

    <p
      v-if="sources.loading"
      class="text-sm text-zinc-500"
    >
      {{ t("common.loading") }}
    </p>
    <p
      v-else-if="sources.sources.length === 0"
      class="text-sm text-zinc-500"
    >
      {{ t("settings.sources.empty") }}
    </p>
    <table
      v-else
      class="w-full text-left text-sm"
    >
      <thead class="text-xs text-zinc-500">
        <tr>
          <th class="py-1">
            {{ t("settings.sources.column.name") }}
          </th>
          <th class="py-1">
            {{ t("settings.sources.column.enabled") }}
          </th>
          <th class="py-1">
            {{ t("settings.sources.column.localPath") }}
          </th>
          <th class="py-1">
            {{ t("settings.sources.column.driveDestination") }}
          </th>
          <th class="py-1">
            {{ t("settings.sources.column.encryption") }}
          </th>
          <th class="py-1">
            {{ t("settings.sources.column.actions") }}
          </th>
        </tr>
      </thead>
      <tbody class="divide-y">
        <tr
          v-for="source in sources.sources"
          :key="source.id"
        >
          <td class="py-2">
            {{ source.displayName }}
          </td>
          <td class="py-2">
            {{ source.enabled ? t("common.yes") : t("common.no") }}
          </td>
          <td class="py-2">
            {{ source.localPath }}
          </td>
          <td class="py-2">
            {{ source.driveFolderPath }}
          </td>
          <td class="py-2">
            {{ source.encryptionEnabled ? t("common.yes") : t("common.no") }}
          </td>
          <td class="py-2">
            <button
              type="button"
              class="rounded border px-2 py-1 text-xs"
            >
              {{ t("settings.sources.runNowButton") }}
            </button>
          </td>
        </tr>
      </tbody>
    </table>

    <AddSourceWizard />
  </div>
</template>
