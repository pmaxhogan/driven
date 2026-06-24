<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import RecoveryPhraseReveal from "./RecoveryPhraseReveal.vue";
import * as ipc from "../ipc/commands";
import { useAccountsStore } from "../stores/accounts";
import { useSourcesStore } from "../stores/sources";
import type {
  DriveFolderEntry,
  ExclusionPreview,
  SourceDto,
} from "../ipc/types";

// Add-source wizard (SPEC s11.2; DESIGN s8.5 step 3 / s8.2 add-source wizard).
// Five steps: pick a LOCAL folder (tauri-plugin-dialog, dialog-derived path
// only - the webview is never trusted to supply an arbitrary local path), pick
// a DRIVE destination (pick_drive_folder paginated tree under the chosen
// account), preview EXCLUSIONS (preview_exclusions: first ~50 included vs
// excluded), opt into ENCRYPTION, then CONFIRM (add_source). The modal is closed
// by default; the parent SourceTable opens it via the exposed `start()`.
const { t, locale } = useI18n();
const accounts = useAccountsStore();
const sources = useSourcesStore();

const emit = defineEmits<{ created: [source: SourceDto] }>();

type Step = "localFolder" | "driveFolder" | "exclusions" | "encryption" | "confirm";
const STEPS: Step[] = [
  "localFolder",
  "driveFolder",
  "exclusions",
  "encryption",
  "confirm",
];

const open = ref(false);
const stepIndex = ref(0);
const step = computed<Step>(() => STEPS[stepIndex.value]);

// Form state.
const accountId = ref<string | null>(null);
// `localPath` is ONLY ever set from the dialog result (dialog-derived); there is
// no text input for it, so the webview cannot inject an arbitrary path.
const localPath = ref<string | null>(null);
const driveFolderId = ref<string | null>(null);
const driveFolderPath = ref<string>("");
const respectGitignore = ref(true);
const includePatternsText = ref("");
const excludePatternsText = ref("");
const encryptionEnabled = ref(false);
const phraseConfirmed = ref(false);
// The backend returns the BIP39 phrase on encryption opt-in; until that path is
// wired it stays empty and the reveal is inert. Never fabricated here.
const recoveryPhrase = ref<string[]>([]);

// Drive picker state: a breadcrumb stack of the folders descended into, so "up"
// can re-fetch the parent. The first entry (null id) is the Drive root.
interface Crumb {
  id: string | null;
  path: string;
}
const crumbs = ref<Crumb[]>([]);
const driveFolders = ref<DriveFolderEntry[]>([]);
const drivePickerLoading = ref(false);

const preview = ref<ExclusionPreview | null>(null);
const previewLoading = ref(false);
const submitting = ref(false);
const errorMessage = ref<string | null>(null);

const includePatterns = computed(() => splitPatterns(includePatternsText.value));
const excludePatterns = computed(() => splitPatterns(excludePatternsText.value));

const canLeaveLocal = computed(
  () => accountId.value !== null && localPath.value !== null,
);
const canLeaveDrive = computed(() => driveFolderId.value !== null);
const canLeaveEncryption = computed(
  () => !encryptionEnabled.value || phraseConfirmed.value,
);

const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

function splitPatterns(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
}

function formatBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded =
    unit === 0
      ? value.toString()
      : value.toLocaleString(locale.value, { maximumFractionDigits: 1 });
  return `${rounded} ${units[unit]}`;
}

async function start(): Promise<void> {
  reset();
  open.value = true;
  await accounts.refresh();
  if (accounts.accounts.length > 0) {
    accountId.value = accounts.accounts[0].id;
  }
}

function reset(): void {
  stepIndex.value = 0;
  accountId.value = null;
  localPath.value = null;
  driveFolderId.value = null;
  driveFolderPath.value = "";
  respectGitignore.value = true;
  includePatternsText.value = "";
  excludePatternsText.value = "";
  encryptionEnabled.value = false;
  phraseConfirmed.value = false;
  recoveryPhrase.value = [];
  crumbs.value = [];
  driveFolders.value = [];
  preview.value = null;
  errorMessage.value = null;
  submitting.value = false;
}

function close(): void {
  open.value = false;
}

async function chooseLocalFolder(): Promise<void> {
  errorMessage.value = null;
  const selected = await openDialog({ directory: true, multiple: false });
  // `open` returns null on cancel; a string for a single directory. We never
  // accept a typed path, only this dialog result.
  if (typeof selected === "string") {
    localPath.value = selected;
  }
}

async function loadDriveFolder(crumb: Crumb): Promise<void> {
  if (accountId.value === null) return;
  drivePickerLoading.value = true;
  errorMessage.value = null;
  try {
    const listing = await ipc.pickDriveFolder(accountId.value, crumb.id);
    driveFolders.value = listing.folders;
    driveFolderId.value = listing.currentFolderId;
    driveFolderPath.value = listing.currentFolderPath;
  } catch (e) {
    errorMessage.value = String(e);
  } finally {
    drivePickerLoading.value = false;
  }
}

async function openDriveRoot(): Promise<void> {
  crumbs.value = [{ id: null, path: "" }];
  await loadDriveFolder(crumbs.value[0]);
}

async function descendInto(folder: DriveFolderEntry): Promise<void> {
  const parentPath = driveFolderPath.value;
  const crumb: Crumb = {
    id: folder.id,
    path: parentPath ? `${parentPath}/${folder.name}` : folder.name,
  };
  crumbs.value.push(crumb);
  await loadDriveFolder(crumb);
}

async function goToCrumb(index: number): Promise<void> {
  crumbs.value = crumbs.value.slice(0, index + 1);
  await loadDriveFolder(crumbs.value[index]);
}

async function loadPreview(): Promise<void> {
  if (localPath.value === null) return;
  previewLoading.value = true;
  errorMessage.value = null;
  try {
    preview.value = await ipc.previewExclusions({
      localPath: localPath.value,
      respectGitignore: respectGitignore.value,
      includePatterns: includePatterns.value,
      excludePatterns: excludePatterns.value,
    });
  } catch (e) {
    errorMessage.value = String(e);
  } finally {
    previewLoading.value = false;
  }
}

async function next(): Promise<void> {
  if (stepIndex.value >= STEPS.length - 1) return;
  stepIndex.value += 1;
  // Lazily load each step's data as it becomes active.
  if (step.value === "driveFolder" && crumbs.value.length === 0) {
    await openDriveRoot();
  } else if (step.value === "exclusions") {
    await loadPreview();
  }
}

function back(): void {
  if (stepIndex.value > 0) stepIndex.value -= 1;
}

async function confirm(): Promise<void> {
  if (
    accountId.value === null ||
    localPath.value === null ||
    driveFolderId.value === null
  ) {
    return;
  }
  submitting.value = true;
  errorMessage.value = null;
  try {
    const displayName = localPath.value.split(/[\\/]/).filter(Boolean).pop();
    const created = await sources.add({
      accountId: accountId.value,
      displayName: displayName ?? localPath.value,
      localPath: localPath.value,
      driveFolderId: driveFolderId.value,
      driveFolderPath: driveFolderPath.value,
      encryptionEnabled: encryptionEnabled.value,
      respectGitignore: respectGitignore.value,
      includePatterns: includePatterns.value,
      excludePatterns: excludePatterns.value,
    });
    emit("created", created);
    close();
  } catch (e) {
    errorMessage.value = String(e);
  } finally {
    submitting.value = false;
  }
}

defineExpose({ start });
</script>

<template>
  <div
    v-if="open"
    class="fixed inset-0 flex items-center justify-center bg-black/40"
  >
    <div
      class="w-full max-w-lg space-y-4 rounded bg-white p-6 dark:bg-zinc-900"
    >
      <h2 class="text-lg font-medium">
        {{ t("settings.addSource.title") }}
      </h2>

      <ol class="flex flex-wrap gap-2 text-xs text-zinc-500">
        <li
          v-for="(s, i) in STEPS"
          :key="s"
          :class="i === stepIndex ? 'font-medium text-zinc-900 dark:text-zinc-100' : ''"
        >
          {{ t(`settings.addSource.step.${s}`) }}
        </li>
      </ol>

      <!-- Step 1: local folder + account -->
      <div
        v-if="step === 'localFolder'"
        class="space-y-3"
      >
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.sources.column.account")
          }}</span>
          <select
            v-model="accountId"
            class="w-full rounded border px-2 py-1.5 text-sm"
          >
            <option
              v-for="account in accounts.accounts"
              :key="account.id"
              :value="account.id"
            >
              {{ account.email }}
            </option>
          </select>
        </label>

        <button
          type="button"
          class="rounded border px-3 py-1.5 text-sm"
          @click="chooseLocalFolder"
        >
          {{ t("settings.addSource.chooseLocalButton") }}
        </button>
        <p
          v-if="localPath"
          class="break-all text-sm text-zinc-600 dark:text-zinc-400"
          data-testid="local-path"
        >
          {{ localPath }}
        </p>
      </div>

      <!-- Step 2: Drive folder picker -->
      <div
        v-else-if="step === 'driveFolder'"
        class="space-y-3"
      >
        <nav class="flex flex-wrap items-center gap-1 text-xs">
          <button
            v-for="(crumb, i) in crumbs"
            :key="i"
            type="button"
            class="rounded px-1 py-0.5 hover:underline"
            @click="goToCrumb(i)"
          >
            {{ i === 0 ? t("settings.addSource.step.driveFolder") : crumb.path.split("/").pop() }}
          </button>
        </nav>
        <p
          v-if="drivePickerLoading"
          class="text-sm text-zinc-500"
        >
          {{ t("common.loading") }}
        </p>
        <ul
          v-else
          class="max-h-56 divide-y overflow-auto rounded border"
        >
          <li
            v-for="folder in driveFolders"
            :key="folder.id"
          >
            <button
              type="button"
              class="w-full px-3 py-2 text-left text-sm hover:bg-zinc-100 dark:hover:bg-zinc-800"
              @click="descendInto(folder)"
            >
              {{ folder.name }}
            </button>
          </li>
        </ul>
        <p class="text-sm text-zinc-600 dark:text-zinc-400">
          {{ t("settings.addSource.step.driveFolder") }}: {{ driveFolderPath }}
        </p>
      </div>

      <!-- Step 3: exclusions preview -->
      <div
        v-else-if="step === 'exclusions'"
        class="space-y-3"
      >
        <label class="flex items-center gap-2 text-sm">
          <input
            v-model="respectGitignore"
            type="checkbox"
            @change="loadPreview"
          >
          {{ t("settings.addSource.respectGitignoreLabel") }}
        </label>
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.addSource.includePatternsLabel")
          }}</span>
          <textarea
            v-model="includePatternsText"
            rows="2"
            class="w-full rounded border px-2 py-1 text-sm"
            @blur="loadPreview"
          />
        </label>
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.addSource.excludePatternsLabel")
          }}</span>
          <textarea
            v-model="excludePatternsText"
            rows="2"
            class="w-full rounded border px-2 py-1 text-sm"
            @blur="loadPreview"
          />
        </label>

        <p
          v-if="previewLoading"
          class="text-sm text-zinc-500"
        >
          {{ t("common.loading") }}
        </p>
        <div
          v-else-if="preview"
          class="space-y-2 text-sm"
          data-testid="exclusion-preview"
        >
          <p>
            {{
              t("settings.addSource.preview.included", {
                count: numberFormatter.format(preview.includedCount),
              })
            }}
            -
            {{
              t("settings.addSource.preview.includedBytes", {
                size: formatBytes(preview.includedBytes),
              })
            }}
          </p>
          <p>
            {{
              t("settings.addSource.preview.excluded", {
                count: numberFormatter.format(preview.excludedCount),
              })
            }}
          </p>
          <div class="grid grid-cols-2 gap-3">
            <ul class="max-h-40 overflow-auto text-xs text-zinc-600 dark:text-zinc-400">
              <li
                v-for="(path, i) in preview.includedSample"
                :key="`inc-${i}`"
                class="break-all"
              >
                {{ path }}
              </li>
            </ul>
            <ul class="max-h-40 overflow-auto text-xs text-zinc-400 line-through">
              <li
                v-for="(path, i) in preview.excludedSample"
                :key="`exc-${i}`"
                class="break-all"
              >
                {{ path }}
              </li>
            </ul>
          </div>
          <p
            v-if="preview.truncated"
            class="text-xs text-zinc-500"
          >
            {{ t("settings.addSource.preview.truncated") }}
          </p>
        </div>
      </div>

      <!-- Step 4: encryption opt-in -->
      <div
        v-else-if="step === 'encryption'"
        class="space-y-3"
      >
        <label class="flex items-center gap-2 text-sm">
          <input
            v-model="encryptionEnabled"
            type="checkbox"
          >
          {{ t("wizard.step4.enableLabel") }}
        </label>
        <p
          v-if="encryptionEnabled"
          class="text-xs text-amber-700 dark:text-amber-400"
        >
          {{ t("wizard.step4.recoveryWarning") }}
        </p>
        <RecoveryPhraseReveal
          v-if="encryptionEnabled"
          v-model:confirmed="phraseConfirmed"
          :phrase="recoveryPhrase"
        />
      </div>

      <!-- Step 5: confirm -->
      <div
        v-else
        class="space-y-2 text-sm"
        data-testid="confirm-summary"
      >
        <p>
          {{ t("settings.addSource.step.localFolder") }}: {{ localPath }}
        </p>
        <p>
          {{ t("settings.addSource.step.driveFolder") }}: {{ driveFolderPath }}
        </p>
        <p>
          {{ t("settings.sources.column.encryption") }}:
          {{ encryptionEnabled ? t("common.enabled") : t("common.disabled") }}
        </p>
      </div>

      <p
        v-if="errorMessage"
        class="text-sm text-red-600"
      >
        {{ errorMessage }}
      </p>

      <div class="flex justify-between gap-2">
        <button
          type="button"
          class="rounded border px-3 py-1.5 text-sm"
          @click="close"
        >
          {{ t("common.cancel") }}
        </button>
        <div class="flex gap-2">
          <button
            v-if="stepIndex > 0"
            type="button"
            class="rounded border px-3 py-1.5 text-sm"
            @click="back"
          >
            {{ t("common.back") }}
          </button>
          <button
            v-if="step !== 'confirm'"
            type="button"
            class="rounded border px-3 py-1.5 text-sm"
            :disabled="
              (step === 'localFolder' && !canLeaveLocal) ||
                (step === 'driveFolder' && !canLeaveDrive) ||
                (step === 'encryption' && !canLeaveEncryption)
            "
            @click="next"
          >
            {{ t("common.next") }}
          </button>
          <button
            v-else
            type="button"
            class="rounded border px-3 py-1.5 text-sm"
            :disabled="submitting"
            @click="confirm"
          >
            {{ t("common.finish") }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>
