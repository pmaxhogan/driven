<script setup lang="ts">
import { onMounted } from "vue";
import { useI18n } from "vue-i18n";

import type { BackendDto, BackendKindId } from "../ipc/types";

// Backup-destination picker (DESIGN s8.5 step 1). Chooses WHICH remote store the
// account being created backs up to.
//
// The option list is NOT hard-coded here: it is whatever the Rust factory
// (`driven_backend::descriptors()`) reports over `list_backends`, so this
// component can never offer a destination the running binary cannot construct.
// A build that ships one destination renders one card; the same markup gains a
// row per destination as backends land, with no change here.
//
// Google Drive is preselected: it is the factory's default descriptor, it is what
// every existing account uses, and the store seeds `backendId` with it before the
// list even resolves - so a user who never touches this step gets exactly the
// behaviour Driven has always had.
//
// Labels are i18n keys derived from the id (`backendPicker.kind.<id>.name` /
// `.description`), with a graceful fallback to the raw id so a destination added
// on the Rust side is never rendered blank if its strings are not seeded yet.

const { t, te } = useI18n();

const props = defineProps<{
  backends: BackendDto[];
  loading?: boolean;
}>();

const emit = defineEmits<{ (e: "load"): void }>();

const selected = defineModel<BackendKindId>("selected", { required: true });

function nameFor(backend: BackendDto): string {
  const key = `backendPicker.kind.${backend.id}.name`;
  return te(key) ? t(key) : backend.id;
}

function descriptionFor(backend: BackendDto): string {
  const key = `backendPicker.kind.${backend.id}.description`;
  return te(key) ? t(key) : "";
}

function select(id: BackendKindId): void {
  selected.value = id;
}

// Ask the parent to load the descriptor list on mount. The parent owns the IPC
// call (and its error surface) so this component stays presentational and
// trivially mountable in a unit test with no backend.
onMounted(() => {
  if (props.backends.length === 0) emit("load");
});
</script>

<template>
  <fieldset class="space-y-3" data-testid="backend-picker">
    <legend class="text-sm font-medium text-zinc-700 dark:text-zinc-200">
      {{ t("backendPicker.legend") }}
    </legend>
    <p class="text-xs text-zinc-500 dark:text-zinc-400">
      {{ t("backendPicker.help") }}
    </p>

    <p v-if="loading && backends.length === 0" class="text-sm text-zinc-500">
      {{ t("common.loading") }}
    </p>
    <p
      v-else-if="backends.length === 0"
      class="rounded-md border border-dashed border-zinc-300 px-3 py-2 text-sm text-zinc-500 dark:border-zinc-700"
      data-testid="backend-picker-empty"
    >
      {{ t("backendPicker.empty") }}
    </p>

    <ul v-else class="space-y-2">
      <li v-for="backend in backends" :key="backend.id">
        <label
          class="flex cursor-pointer items-start gap-3 rounded-md border px-3 py-2 transition-colors"
          :class="
            selected === backend.id
              ? 'border-teal-500 bg-teal-50 dark:border-teal-400 dark:bg-zinc-800'
              : 'border-zinc-200 hover:bg-zinc-50 dark:border-zinc-700 dark:hover:bg-zinc-800'
          "
          :data-testid="`backend-option-${backend.id}`"
        >
          <input
            type="radio"
            name="backup-destination"
            class="mt-1"
            :value="backend.id"
            :checked="selected === backend.id"
            @change="select(backend.id)"
          />
          <span class="min-w-0">
            <span class="block text-sm font-medium text-zinc-800 dark:text-zinc-100">
              {{ nameFor(backend) }}
            </span>
            <span
              v-if="descriptionFor(backend)"
              class="block text-xs text-zinc-500 dark:text-zinc-400"
            >
              {{ descriptionFor(backend) }}
            </span>
          </span>
        </label>
      </li>
    </ul>
  </fieldset>
</template>
