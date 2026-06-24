<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import AddSourceWizard from "./AddSourceWizard.vue";
import * as ipc from "../ipc/commands";
import { useAccountsStore } from "../stores/accounts";
import { useSourcesStore } from "../stores/sources";
import type { ExclusionPreview, SourceDto } from "../ipc/types";

// Sources settings tab body (SPEC s11.2; DESIGN s8.2). A table of sources with
// the per-row affordances the design calls for: enabled toggle, local path,
// Drive destination, account, encryption on/off, "Edit exclusions" (inline
// editor with a live preview), "Run now" (sync_now), and "Remove" (with an
// "also delete from Drive" opt-in). "Add source" opens the AddSourceWizard.
const { t, locale } = useI18n();
const sources = useSourcesStore();
const accounts = useAccountsStore();

const wizard = ref<InstanceType<typeof AddSourceWizard> | null>(null);

// Inline exclusion-editor state, keyed by the source being edited.
const editingId = ref<string | null>(null);
const editRespectGitignore = ref(true);
const editIncludeText = ref("");
const editExcludeText = ref("");
const editPreview = ref<ExclusionPreview | null>(null);
const editPreviewLoading = ref(false);
const savingEdit = ref(false);

// Inline remove-confirmation state.
const confirmingRemoveId = ref<string | null>(null);
const deleteRemote = ref(false);

const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

const accountEmailById = computed<Record<string, string>>(() => {
  const map: Record<string, string> = {};
  for (const account of accounts.accounts) {
    map[account.id] = account.email;
  }
  return map;
});

onMounted(async () => {
  await Promise.all([sources.refresh(), accounts.refresh()]);
});

function splitPatterns(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
}

function openWizard(): void {
  void wizard.value?.start();
}

async function toggleEnabled(source: SourceDto): Promise<void> {
  await sources.update(source.id, { enabled: !source.enabled });
}

async function runNow(source: SourceDto): Promise<void> {
  await sources.syncNow(source.id);
}

function beginEditExclusions(source: SourceDto): void {
  editingId.value = source.id;
  editRespectGitignore.value = source.respectGitignore;
  editIncludeText.value = source.includePatterns.join("\n");
  editExcludeText.value = source.excludePatterns.join("\n");
  editPreview.value = null;
  void loadEditPreview(source);
}

function cancelEdit(): void {
  editingId.value = null;
  editPreview.value = null;
}

async function loadEditPreview(source: SourceDto): Promise<void> {
  editPreviewLoading.value = true;
  try {
    // R1-P1-2 (SPEC s11.6.1): preview an EXISTING source by its id - the backend
    // resolves the local path from SQLite, never from a webview-supplied string.
    editPreview.value = await ipc.previewExclusions({
      sourceId: source.id,
      respectGitignore: editRespectGitignore.value,
      includePatterns: splitPatterns(editIncludeText.value),
      excludePatterns: splitPatterns(editExcludeText.value),
    });
  } finally {
    editPreviewLoading.value = false;
  }
}

async function saveEdit(source: SourceDto): Promise<void> {
  savingEdit.value = true;
  try {
    await sources.update(source.id, {
      respectGitignore: editRespectGitignore.value,
      includePatterns: splitPatterns(editIncludeText.value),
      excludePatterns: splitPatterns(editExcludeText.value),
    });
    editingId.value = null;
    editPreview.value = null;
  } finally {
    savingEdit.value = false;
  }
}

function beginRemove(sourceId: string): void {
  confirmingRemoveId.value = sourceId;
  deleteRemote.value = false;
}

function cancelRemove(): void {
  confirmingRemoveId.value = null;
  deleteRemote.value = false;
}

async function confirmRemove(sourceId: string): Promise<void> {
  await sources.remove(sourceId, deleteRemote.value);
  confirmingRemoveId.value = null;
  deleteRemote.value = false;
}
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
        @click="openWizard"
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
      v-else-if="sources.error"
      class="text-sm text-red-600"
    >
      {{ sources.error }}
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
            {{ t("settings.sources.column.account") }}
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
        <template
          v-for="source in sources.sources"
          :key="source.id"
        >
          <tr>
            <td class="py-2">
              {{ source.displayName }}
            </td>
            <td class="py-2">
              <input
                type="checkbox"
                :checked="source.enabled"
                :aria-label="t('settings.sources.column.enabled')"
                @change="toggleEnabled(source)"
              >
            </td>
            <td class="break-all py-2">
              {{ source.localPath }}
            </td>
            <td class="break-all py-2">
              {{ source.driveFolderPath }}
            </td>
            <td class="break-all py-2">
              {{ accountEmailById[source.accountId] ?? source.accountId }}
            </td>
            <td class="py-2">
              {{ source.encryptionEnabled ? t("common.yes") : t("common.no") }}
            </td>
            <td class="py-2">
              <div class="flex flex-wrap gap-1">
                <button
                  type="button"
                  class="rounded border px-2 py-1 text-xs"
                  @click="beginEditExclusions(source)"
                >
                  {{ t("settings.sources.editExclusionsButton") }}
                </button>
                <button
                  type="button"
                  class="rounded border px-2 py-1 text-xs"
                  @click="runNow(source)"
                >
                  {{ t("settings.sources.runNowButton") }}
                </button>
                <button
                  type="button"
                  class="rounded border px-2 py-1 text-xs"
                  @click="beginRemove(source.id)"
                >
                  {{ t("settings.sources.removeButton") }}
                </button>
              </div>
            </td>
          </tr>

          <tr v-if="editingId === source.id">
            <td
              colspan="7"
              class="py-2"
            >
              <div
                class="space-y-2 rounded border p-3"
                data-testid="exclusion-editor"
              >
                <label class="flex items-center gap-2 text-sm">
                  <input
                    v-model="editRespectGitignore"
                    type="checkbox"
                    @change="loadEditPreview(source)"
                  >
                  {{ t("settings.addSource.respectGitignoreLabel") }}
                </label>
                <label class="block space-y-1 text-sm">
                  <span class="text-zinc-600 dark:text-zinc-400">{{
                    t("settings.addSource.includePatternsLabel")
                  }}</span>
                  <textarea
                    v-model="editIncludeText"
                    rows="2"
                    class="w-full rounded border px-2 py-1 text-sm"
                    @blur="loadEditPreview(source)"
                  />
                </label>
                <label class="block space-y-1 text-sm">
                  <span class="text-zinc-600 dark:text-zinc-400">{{
                    t("settings.addSource.excludePatternsLabel")
                  }}</span>
                  <textarea
                    v-model="editExcludeText"
                    rows="2"
                    class="w-full rounded border px-2 py-1 text-sm"
                    @blur="loadEditPreview(source)"
                  />
                </label>
                <p
                  v-if="editPreviewLoading"
                  class="text-sm text-zinc-500"
                >
                  {{ t("common.loading") }}
                </p>
                <p
                  v-else-if="editPreview"
                  class="text-sm"
                >
                  {{
                    t("settings.addSource.preview.included", {
                      count: numberFormatter.format(editPreview.includedCount),
                    })
                  }}
                  -
                  {{
                    t("settings.addSource.preview.excluded", {
                      count: numberFormatter.format(editPreview.excludedCount),
                    })
                  }}
                </p>
                <div class="flex gap-2">
                  <button
                    type="button"
                    class="rounded border px-3 py-1.5 text-sm"
                    :disabled="savingEdit"
                    @click="saveEdit(source)"
                  >
                    {{ t("common.save") }}
                  </button>
                  <button
                    type="button"
                    class="rounded border px-3 py-1.5 text-sm"
                    @click="cancelEdit"
                  >
                    {{ t("common.cancel") }}
                  </button>
                </div>
              </div>
            </td>
          </tr>

          <tr v-if="confirmingRemoveId === source.id">
            <td
              colspan="7"
              class="py-2"
            >
              <div
                class="space-y-2 rounded border border-red-300 bg-red-50 p-3 text-sm dark:bg-red-950/30"
                data-testid="source-remove-confirm"
              >
                <label class="flex items-center gap-2">
                  <input
                    v-model="deleteRemote"
                    type="checkbox"
                  >
                  {{ t("settings.sources.deleteRemoteLabel") }}
                </label>
                <div class="flex gap-2">
                  <button
                    type="button"
                    class="rounded border border-red-400 px-2 py-1 text-xs text-red-700"
                    @click="confirmRemove(source.id)"
                  >
                    {{ t("settings.sources.removeButton") }}
                  </button>
                  <button
                    type="button"
                    class="rounded border px-2 py-1 text-xs"
                    @click="cancelRemove"
                  >
                    {{ t("common.cancel") }}
                  </button>
                </div>
              </div>
            </td>
          </tr>
        </template>
      </tbody>
    </table>

    <AddSourceWizard
      ref="wizard"
      @created="sources.refresh()"
    />
  </div>
</template>
