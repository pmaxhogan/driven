<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

import { pickFolderDialog } from "../ipc/commands";
import type { CreateLocalFolderAccountRequest } from "../ipc/types";

// The local / removable-folder destination's setup step (the non-OAuth branch of
// DESIGN s8.5 step 2). Collects one thing - the destination folder - and emits a
// request for the parent to hand to `create_local_folder_account`.
//
// There is no credential field and there never will be: a folder the user
// already has write access to needs none, so this backend touches the OS
// keychain not at all.
//
// SPEC s11.6.1 / C1: the folder is chosen through the BACKEND-owned native
// dialog, never typed. That is not only better UX - it is the only way the
// backend can be sure the path was user-chosen rather than webview-supplied.
// (The account command re-validates the path anyway; nothing here is trusted.)
//
// Validation here is presentational only - enough to keep the submit button
// honest. The REAL validation is the backend's: it proves the folder exists, is
// a directory, and accepts a real write BEFORE any account row is written, so a
// disconnected drive or a read-only mount surfaces a specific error instead of
// an account that silently never backs anything up.

const { t } = useI18n();

const props = defineProps<{
  busy?: boolean;
  /** A stable SPEC s24 error message from a failed submit, already localized by
   * the parent (which owns the error vocabulary). */
  errorMessage?: string | null;
}>();

const emit = defineEmits<{ (e: "submit", req: CreateLocalFolderAccountRequest): void }>();

const root = ref("");
const displayName = ref("");
const picking = ref(false);

const canSubmit = computed(() => !props.busy && !picking.value && root.value.trim().length > 0);

async function chooseFolder(): Promise<void> {
  picking.value = true;
  try {
    const picked = await pickFolderDialog();
    root.value = picked.path;
    if (!displayName.value) displayName.value = baseName(picked.path);
  } catch {
    // A cancel (or a dialog failure) surfaces as a rejected command. Leave the
    // path unset so the submit button stays disabled - a cancel is not an error
    // worth shouting about.
  } finally {
    picking.value = false;
  }
}

function submit(): void {
  if (!canSubmit.value) return;
  emit("submit", {
    root: root.value.trim(),
    displayName: displayName.value.trim() || null,
  });
}

/** Last path component, for the default label. Handles both separators so a
 * Windows path renders the same as a POSIX one. */
function baseName(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : p;
}

const FIELD =
  "w-full rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100";
const LABEL = "block text-sm font-medium text-zinc-700 dark:text-zinc-200";
const HINT = "text-xs text-zinc-500 dark:text-zinc-400";
const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
</script>

<template>
  <form class="space-y-4" data-testid="local-folder-form" @submit.prevent="submit">
    <p class="text-zinc-600 dark:text-zinc-400">
      {{ t("localFolderSetup.body") }}
    </p>

    <div class="space-y-2">
      <span :class="LABEL">{{ t("localFolderSetup.folderLabel") }}</span>
      <button
        type="button"
        :class="SECONDARY_BTN"
        :disabled="picking || busy"
        data-testid="local-folder-choose"
        @click="chooseFolder"
      >
        {{ t("localFolderSetup.chooseButton") }}
      </button>
      <p
        v-if="root"
        class="break-all text-sm text-zinc-600 dark:text-zinc-400"
        data-testid="local-folder-path"
      >
        {{ root }}
      </p>
      <p :class="HINT">{{ t("localFolderSetup.folderHint") }}</p>
    </div>

    <div class="space-y-1">
      <label :class="LABEL" for="local-folder-name">{{
        t("localFolderSetup.displayNameLabel")
      }}</label>
      <input
        id="local-folder-name"
        v-model="displayName"
        :class="FIELD"
        type="text"
        autocomplete="off"
        data-testid="local-folder-name"
      />
      <p :class="HINT">{{ t("localFolderSetup.displayNameHint") }}</p>
    </div>

    <p class="text-xs text-amber-700 dark:text-amber-400" data-testid="local-folder-trash-warning">
      {{ t("localFolderSetup.trashWarning") }}
    </p>
    <p class="text-xs text-amber-700 dark:text-amber-400">
      {{ t("localFolderSetup.removableWarning") }}
    </p>

    <p v-if="errorMessage" class="text-sm text-red-600 dark:text-red-400" role="alert">
      {{ errorMessage }}
    </p>

    <button
      type="submit"
      class="inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50"
      :disabled="!canSubmit"
      data-testid="local-folder-connect"
    >
      {{ busy ? t("localFolderSetup.connecting") : t("localFolderSetup.connectButton") }}
    </button>
  </form>
</template>
