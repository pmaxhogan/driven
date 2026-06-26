<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

import RecoveryPhraseReveal from "./RecoveryPhraseReveal.vue";
import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { useAccountsStore } from "../stores/accounts";
import { useSourcesStore } from "../stores/sources";
import type { DriveFolderEntry, ExclusionPreview, SourceDto } from "../ipc/types";

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

// B3: a post-confirm "reveal" step is appended when an encrypted add returned a
// recovery phrase; the user must acknowledge it before the wizard closes.
type Step = "localFolder" | "driveFolder" | "exclusions" | "encryption" | "confirm" | "reveal";
const STEPS: Step[] = ["localFolder", "driveFolder", "exclusions", "encryption", "confirm"];

const open = ref(false);
const stepIndex = ref(0);
// B3: the reveal step is shown out-of-band (after a successful encrypted add),
// so it is tracked separately rather than as a normal STEPS index.
const revealing = ref(false);
const step = computed<Step>(() => (revealing.value ? "reveal" : STEPS[stepIndex.value]));

// Form state.
const accountId = ref<string | null>(null);
// `localPath` is ONLY ever set from the BACKEND folder dialog (dialog-derived);
// there is no text input for it, so the webview cannot inject an arbitrary path.
const localPath = ref<string | null>(null);
// C1: the one-shot dialog token bound to the chosen local folder (required by
// add_source so the backend can prove the path is dialog-derived).
const localPathToken = ref<string | null>(null);
const driveFolderId = ref<string | null>(null);
const driveFolderPath = ref<string>("");
const respectGitignore = ref(true);
const includePatternsText = ref("");
const excludePatternsText = ref("");
const encryptionEnabled = ref(false);
const phraseConfirmed = ref(false);
// R3-P1-1: the user has actually REVEALED the phrase at least once. The reveal
// step's Done button gates on reveal AND acknowledge so the phrase can never be
// confirmed-without-seeing-it. Reset whenever the phrase changes/clears.
const phraseRevealed = ref(false);
// B3: the BIP39 phrase the backend RETURNS from add_source on the first
// encrypted source. Empty until then; shown once on the reveal step.
const recoveryPhrase = ref<string[]>([]);
// B3: the source created on confirm (held so the reveal step can emit it after
// the phrase is acknowledged).
const createdSource = ref<SourceDto | null>(null);
// M9c D4 (M6 R4-P1-1, DATA-SAFETY): true when the created source was persisted
// DISABLED and awaits a backend recovery-phrase ack. The reveal-step Done button
// then calls ackRecoveryPhraseSaved (which enables the source); the reveal button
// calls revealRecoveryPhrase (the backend reveal the ack gate requires).
const pendingRecoveryAck = ref(false);

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
// R8-P2-1: the recovery reveal/ack error on the reveal step as a stable SPEC s24
// CODE (not a raw String(e), which renders a Tauri structured error as
// `[object Object]` and can leak backend English). The reveal step localizes it
// via t(`errors.${code}.long`).
const revealErrorCode = ref<string | null>(null);

const includePatterns = computed(() => splitPatterns(includePatternsText.value));
const excludePatterns = computed(() => splitPatterns(excludePatternsText.value));

const canLeaveLocal = computed(() => accountId.value !== null && localPathToken.value !== null);
const canLeaveDrive = computed(() => driveFolderId.value !== null);

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
  revealing.value = false;
  accountId.value = null;
  localPath.value = null;
  localPathToken.value = null;
  driveFolderId.value = null;
  driveFolderPath.value = "";
  respectGitignore.value = true;
  includePatternsText.value = "";
  excludePatternsText.value = "";
  encryptionEnabled.value = false;
  phraseConfirmed.value = false;
  phraseRevealed.value = false;
  recoveryPhrase.value = [];
  createdSource.value = null;
  pendingRecoveryAck.value = false;
  crumbs.value = [];
  driveFolders.value = [];
  preview.value = null;
  errorMessage.value = null;
  revealErrorCode.value = null;
  submitting.value = false;
}

function close(): void {
  open.value = false;
}

async function chooseLocalFolder(): Promise<void> {
  errorMessage.value = null;
  try {
    // C1: the BACKEND owns the folder dialog and returns { path, token }. We
    // never accept a typed path - only this dialog result + its token.
    const picked = await ipc.pickFolderDialog();
    localPath.value = picked.path;
    localPathToken.value = picked.token;
  } catch {
    // A cancel (or dialog error) leaves the path unset so "Next" stays disabled.
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
    // R4-P2-2: the backend cannot derive the full breadcrumb (it lists one
    // folder's children, not the ancestor chain), so it returns an empty
    // `currentFolderPath`. The wizard maintains the breadcrumb itself in the
    // `crumbs` stack (descend appends `parent/name`), so persist THAT path -
    // using the empty backend value here was what left `drive_folder_path` blank
    // in SQLite. Fall back to the backend value only if the crumb has no path
    // (root), keeping "My Drive" root as empty.
    driveFolderPath.value = crumb.path || listing.currentFolderPath;
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
  // R1-P1-2: preview by the backend-minted dialog TOKEN (not a raw path). The
  // token is peeked non-consumingly, so add_source still gets its single use.
  if (localPathToken.value === null) return;
  previewLoading.value = true;
  errorMessage.value = null;
  try {
    preview.value = await ipc.previewExclusions({
      localPathToken: localPathToken.value,
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
    localPathToken.value === null ||
    driveFolderId.value === null
  ) {
    return;
  }
  submitting.value = true;
  errorMessage.value = null;
  try {
    const displayName = localPath.value.split(/[\\/]/).filter(Boolean).pop();
    const result = await sources.add({
      accountId: accountId.value,
      displayName: displayName ?? localPath.value,
      localPathToken: localPathToken.value,
      localPath: localPath.value,
      driveFolderId: driveFolderId.value,
      driveFolderPath: driveFolderPath.value,
      encryptionEnabled: encryptionEnabled.value,
      respectGitignore: respectGitignore.value,
      includePatterns: includePatterns.value,
      excludePatterns: excludePatterns.value,
    });
    createdSource.value = result.source;
    // M9c D4: a pending-ack source was persisted DISABLED; the reveal step's Done
    // calls the backend ack to enable it.
    pendingRecoveryAck.value = result.pendingRecoveryAck;
    // B3: if a recovery phrase was returned (this opt-in generated the master
    // key), show it ONCE on the reveal step and require acknowledgement before
    // closing. Otherwise (unencrypted, or a subsequent encrypted source) finish.
    if (result.recoveryPhrase && result.recoveryPhrase.length > 0) {
      recoveryPhrase.value = result.recoveryPhrase;
      phraseConfirmed.value = false;
      // R3-P1-1: a fresh phrase must be revealed before it can be acknowledged.
      phraseRevealed.value = false;
      revealing.value = true;
    } else {
      emit("created", result.source);
      close();
    }
  } catch (e) {
    errorMessage.value = String(e);
  } finally {
    submitting.value = false;
  }
}

/** B3 + M9c D4: leave the reveal step once the user acknowledged the phrase. When
 * the source is pending a backend recovery-phrase ack, call ackRecoveryPhraseSaved
 * FIRST (it ENABLES the until-now-disabled source); the backend rejects it unless
 * a real reveal was recorded, so the client gate is backed by the server gate.
 * Then emit the (now-enabled) created source + close. */
async function finishReveal(): Promise<void> {
  // R3-P1-1: never leave the reveal step unless the phrase was revealed AND
  // acknowledged.
  if (!phraseConfirmed.value || !phraseRevealed.value) return;
  const created = createdSource.value;
  if (created && pendingRecoveryAck.value) {
    submitting.value = true;
    revealErrorCode.value = null;
    try {
      const enabled = await sources.ackRecoveryPhrase(created.id);
      pendingRecoveryAck.value = false;
      emit("created", enabled);
      close();
    } catch (e) {
      // R8-P2-1: store the stable code; the reveal step localizes it.
      revealErrorCode.value = toErrorCode(e);
    } finally {
      submitting.value = false;
    }
    return;
  }
  if (created) emit("created", created);
  close();
}

// R3-P1-1: the reveal component signals when the phrase has been revealed (or
// re-locked because it changed). When re-locked, also clear the acknowledgement.
function onPhraseRevealed(value: boolean): void {
  phraseRevealed.value = value;
  if (!value) phraseConfirmed.value = false;
}

/** M9c D4: the reveal action threaded into RecoveryPhraseReveal - the BACKEND
 * reveal the ack gate depends on. Only meaningful for a pending-ack source.
 * R9-P1-2: returns the revealed phrase so RecoveryPhraseReveal latches from the
 * return value. Here the `recoveryPhrase` prop is already set (from the add
 * result), so this matches it; returning it keeps the latch deterministic. */
async function revealPhraseAction(): Promise<string[]> {
  const created = createdSource.value;
  if (!created || !pendingRecoveryAck.value) return [];
  return sources.revealRecoveryPhrase(created.id);
}

/** M9c D4 / R8-P2-1: surface a backend reveal error on the reveal step as a
 * stable SPEC s24 code (normalized via toErrorCode), so the template localizes it
 * - never `[object Object]` / leaked backend English. */
function onPhraseRevealError(code: unknown): void {
  revealErrorCode.value = toErrorCode(code);
}

defineExpose({ start });
</script>

<template>
  <div v-if="open" class="fixed inset-0 flex items-center justify-center bg-black/40">
    <div class="w-full max-w-lg space-y-4 rounded bg-white p-6 dark:bg-zinc-900">
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
      <div v-if="step === 'localFolder'" class="space-y-3">
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.sources.column.account")
          }}</span>
          <select v-model="accountId" class="w-full rounded border px-2 py-1.5 text-sm">
            <option v-for="account in accounts.accounts" :key="account.id" :value="account.id">
              {{ account.email }}
            </option>
          </select>
        </label>

        <button type="button" class="rounded border px-3 py-1.5 text-sm" @click="chooseLocalFolder">
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
      <div v-else-if="step === 'driveFolder'" class="space-y-3">
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
        <p v-if="drivePickerLoading" class="text-sm text-zinc-500">
          {{ t("common.loading") }}
        </p>
        <ul v-else class="max-h-56 divide-y overflow-auto rounded border">
          <li v-for="folder in driveFolders" :key="folder.id">
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
      <div v-else-if="step === 'exclusions'" class="space-y-3">
        <label class="flex items-center gap-2 text-sm">
          <input v-model="respectGitignore" type="checkbox" @change="loadPreview" />
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

        <p v-if="previewLoading" class="text-sm text-zinc-500">
          {{ t("common.loading") }}
        </p>
        <div v-else-if="preview" class="space-y-2 text-sm" data-testid="exclusion-preview">
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
              <li v-for="(path, i) in preview.includedSample" :key="`inc-${i}`" class="break-all">
                {{ path }}
              </li>
            </ul>
            <ul class="max-h-40 overflow-auto text-xs text-zinc-400 line-through">
              <li v-for="(path, i) in preview.excludedSample" :key="`exc-${i}`" class="break-all">
                {{ path }}
              </li>
            </ul>
          </div>
          <p v-if="preview.truncated" class="text-xs text-zinc-500">
            {{ t("settings.addSource.preview.truncated") }}
          </p>
        </div>
      </div>

      <!-- Step 4: encryption opt-in (phrase is revealed AFTER confirm, B3) -->
      <div v-else-if="step === 'encryption'" class="space-y-3">
        <label class="flex items-center gap-2 text-sm">
          <input v-model="encryptionEnabled" type="checkbox" />
          {{ t("wizard.step4.enableLabel") }}
        </label>
        <p v-if="encryptionEnabled" class="text-xs text-amber-700 dark:text-amber-400">
          {{ t("wizard.step4.recoveryWarning") }}
        </p>
      </div>

      <!-- Reveal step: shown after an encrypted add returned a recovery phrase.
           The user must acknowledge before the wizard closes (B3). -->
      <div v-else-if="step === 'reveal'" class="space-y-3" data-testid="reveal-step">
        <p class="text-sm text-amber-700 dark:text-amber-400">
          {{ t("wizard.step4.recoveryWarning") }}
        </p>
        <RecoveryPhraseReveal
          v-model:confirmed="phraseConfirmed"
          :phrase="recoveryPhrase"
          :reveal-action="pendingRecoveryAck ? revealPhraseAction : undefined"
          @update:revealed="onPhraseRevealed"
          @reveal-error="onPhraseRevealError"
        />
        <p v-if="revealErrorCode" class="text-sm text-red-600" data-testid="reveal-error">
          {{ t(`errors.${revealErrorCode}.long`) }}
        </p>
      </div>

      <!-- Step 5: confirm -->
      <div v-else class="space-y-2 text-sm" data-testid="confirm-summary">
        <p>{{ t("settings.addSource.step.localFolder") }}: {{ localPath }}</p>
        <p>{{ t("settings.addSource.step.driveFolder") }}: {{ driveFolderPath }}</p>
        <p>
          {{ t("settings.sources.column.encryption") }}:
          {{ encryptionEnabled ? t("common.enabled") : t("common.disabled") }}
        </p>
      </div>

      <p v-if="errorMessage" class="text-sm text-red-600">
        {{ errorMessage }}
      </p>

      <div class="flex justify-between gap-2">
        <button type="button" class="rounded border px-3 py-1.5 text-sm" @click="close">
          {{ t("common.cancel") }}
        </button>
        <div class="flex gap-2">
          <!-- B3 reveal step: a single "Done" button gated on acknowledgement;
               back/next are hidden so the phrase cannot be skipped. -->
          <button
            v-if="step === 'reveal'"
            type="button"
            class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
            :disabled="!phraseConfirmed || !phraseRevealed"
            data-testid="reveal-done"
            @click="finishReveal"
          >
            {{ t("common.done") }}
          </button>
          <template v-else>
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
                (step === 'driveFolder' && !canLeaveDrive)
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
          </template>
        </div>
      </div>
    </div>
  </div>
</template>
