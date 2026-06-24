<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";

// Add-source wizard (SPEC s11.2; DESIGN s8.5 step 3 / s8.2 add-source wizard).
// M6 shell: the multi-step add-source flow (local folder picker -> Drive folder
// picker -> exclusion preview -> encrypt? -> confirm). The sources implementer
// fills in each step's form + wires the tauri-plugin-dialog folder picker and
// the previewExclusions / pickDriveFolder / addSource IPC wrappers. The shell
// is a closed modal by default so it compiles + renders without a backend.
const { t } = useI18n();

const open = ref(false);

function close(): void {
  open.value = false;
}

defineExpose({ open });
</script>

<template>
  <div
    v-if="open"
    class="fixed inset-0 flex items-center justify-center bg-black/40"
  >
    <div class="w-full max-w-lg space-y-4 rounded bg-white p-6 dark:bg-zinc-900">
      <h2 class="text-lg font-medium">
        {{ t("settings.addSource.title") }}
      </h2>
      <div class="flex justify-end gap-2">
        <button
          type="button"
          class="rounded border px-3 py-1.5 text-sm"
          @click="close"
        >
          {{ t("common.cancel") }}
        </button>
      </div>
    </div>
  </div>
</template>
