<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import DriveFolderPicker from "./DriveFolderPicker.vue";
import ExclusionPreviewTree from "./ExclusionPreviewTree.vue";
import RecoveryPhraseReveal from "./RecoveryPhraseReveal.vue";
import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { useAccountsStore } from "../stores/accounts";
import {
  appendPatternLine,
  splitPatterns,
  unconstrainedIncludePatterns,
} from "../stores/exclusionPreview";
import { useSourcesStore } from "../stores/sources";
import { isWindows } from "../platform";
import type { BackendDto, SourceDto } from "../ipc/types";

// Add-source wizard (SPEC s11.2; DESIGN s8.5 step 3 / s8.2 add-source wizard).
// Five steps: pick a LOCAL folder (tauri-plugin-dialog, dialog-derived path
// only - the webview is never trusted to supply an arbitrary local path), pick
// a DRIVE destination (pick_drive_folder paginated tree under the chosen
// account), preview EXCLUSIONS (ExclusionPreviewTree: a live folder tree that
// streams in as the walk runs, with a per-row "+"/"-" that appends the matching
// glob to the patterns below), opt into ENCRYPTION, then CONFIRM (add_source).
// The modal is closed by default; the parent SourceTable opens it via the
// exposed `start()`.
const { t } = useI18n();
const accounts = useAccountsStore();
const sources = useSourcesStore();

// Shared design-system class strings (DRIVEN UI design system). Teal is the
// accent for primary affordances; native controls carry explicit light/dark
// surfaces so they stay readable on a dark-theme OS.
const primaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const secondaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const inputCls =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-hidden focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";

const emit = defineEmits<{ created: [source: SourceDto] }>();

// The OneDrive / cloud-only placeholder policy only DOES anything on Windows, so
// it is not offered elsewhere. Evaluated once: the host OS cannot change while
// the app is running.
const showPlaceholderPolicy = isWindows();

// B3: a post-confirm "reveal" step is appended when an encrypted add returned a
// recovery phrase; the user must acknowledge it before the wizard closes.
type Step = "localFolder" | "driveFolder" | "exclusions" | "encryption" | "confirm" | "reveal";

const open = ref(false);
const stepIndex = ref(0);
// B3: the reveal step is shown out-of-band (after a successful encrypted add),
// so it is tracked separately rather than as a normal STEPS index.
const revealing = ref(false);

// The destination descriptors this build can construct, loaded when the wizard
// opens. They are what says whether the SELECTED ACCOUNT's destination can be
// browsed - a capability `AccountDto` itself does not carry, which is precisely
// why this wizard used to assume every account was Drive-shaped.
const backends = ref<BackendDto[]>([]);

/** The account the new source will belong to. */
const selectedAccount = computed(() => accounts.accounts.find((a) => a.id === accountId.value));

/** Whether that account's destination has a browsable folder tree. Defaults to
 * `true` (the Drive behaviour) while the descriptors are still loading, so the
 * step list never flickers shorter and then longer.
 *
 * Note this is a per-BACKEND capability, not "is it Google Drive": S3 browses key
 * prefixes and so does get the step, while a local or removable folder has its
 * destination fixed when the account is created and does not. */
const destinationIsBrowsable = computed(() => {
  const kind = selectedAccount.value?.backendKind;
  if (kind === undefined) return true;
  return backends.value.find((b) => b.id === kind)?.supportsFolderPicker ?? true;
});

/** The wizard's steps for the CURRENTLY-SELECTED account. A static list showed
 * "Drive folder" to every account and walked non-Drive users into a step with
 * nothing to pick in it; a destination with no browsable tree simply has no
 * destination step. */
const STEPS = computed<Step[]>(() => [
  "localFolder",
  ...(destinationIsBrowsable.value ? (["driveFolder"] as Step[]) : []),
  "exclusions",
  "encryption",
  "confirm",
]);

const step = computed<Step>(() =>
  revealing.value ? "reveal" : (STEPS.value[stepIndex.value] ?? "confirm")
);

// Form state.
const accountId = ref<string | null>(null);
// `localPath` is ONLY ever set from the BACKEND folder dialog (dialog-derived);
// there is no text input for it, so the webview cannot inject an arbitrary path.
const localPath = ref<string | null>(null);
// C1: the one-shot dialog token bound to the chosen local folder (required by
// add_source so the backend can prove the path is dialog-derived).
const localPathToken = ref<string | null>(null);
const driveFolderId = ref<string | null>(null);
// Issue #7: the Google Shared Drive id the chosen destination lives in (null =
// My Drive), owned by the shared DriveFolderPicker.
const driveId = ref<string | null>(null);
const driveFolderPath = ref<string>("");
const respectGitignore = ref(true);
const includePatternsText = ref("");
const excludePatternsText = ref("");
// Issue #4: when true, back up OneDrive / cloud-only placeholder files
// (PlaceholderPolicy "force_download") instead of skipping them (the default).
const backupCloudOnly = ref(false);
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

// Drive destination (id + human path) is owned by the shared DriveFolderPicker
// via v-model; this component only stages the chosen values for add_source.

// The live streaming folder-tree preview on the exclusions step. It owns the
// walk (start / cancel / batched events); this component only tells it when the
// rules changed.
const previewTree = ref<InstanceType<typeof ExclusionPreviewTree> | null>(null);
const submitting = ref(false);
// The wizard's general error as a stable SPEC s24 CODE (not a raw String(e),
// which renders a Tauri structured `{ code, message }` error as the literal
// "[object Object]" and can leak backend English). The template localizes it via
// t(`errors.${code}.long`).
const errorCode = ref<string | null>(null);
// R8-P2-1: the recovery reveal/ack error on the reveal step, same stable-code
// treatment, localized on the reveal step.
const revealErrorCode = ref<string | null>(null);

const includePatterns = computed(() => splitPatterns(includePatternsText.value));
const excludePatterns = computed(() => splitPatterns(excludePatternsText.value));
// The include rules that stop the scanner from pruning excluded folders, so the
// exclusions step can warn about them AS THEY ARE TYPED (the walk itself only
// re-runs on blur, but the guidance should not wait for it).
const unconstrainedIncludes = computed(() =>
  unconstrainedIncludePatterns(includePatternsText.value)
);

const canLeaveLocal = computed(() => accountId.value !== null && localPathToken.value !== null);
// `!== null`, never truthiness: "" is the destination id of a bucket root, a real
// selection that a `!!` check would reject.
const canLeaveDrive = computed(() => driveFolderId.value !== null);

/** Whether a destination valid for this account's backend has been settled: one
 * picked on the destination step, or the account's own fixed root when its
 * destination cannot be browsed. */
const destinationSettled = computed(() => !destinationIsBrowsable.value || canLeaveDrive.value);

async function start(): Promise<void> {
  reset();
  open.value = true;
  // Load the descriptors and the accounts together; the descriptors decide
  // whether the destination step exists for whichever account is selected.
  await Promise.all([accounts.refresh(), loadBackends()]);
  if (accounts.accounts.length > 0) {
    accountId.value = accounts.accounts[0].id;
  }
}

/** Load the destination descriptors. A failure leaves the list empty, which falls
 * back to the Drive-shaped behaviour rather than dead-ending the wizard. */
async function loadBackends(): Promise<void> {
  try {
    const list = await ipc.listBackends();
    backends.value = Array.isArray(list) ? list : [];
  } catch {
    backends.value = [];
  }
}

// Switching accounts mid-wizard invalidates any destination already picked - a
// Drive folder id means nothing to an S3 account - so drop it and return to the
// first step, whose step list is the one that just changed.
watch(accountId, () => {
  driveFolderId.value = null;
  driveId.value = null;
  driveFolderPath.value = "";
  if (stepIndex.value > STEPS.value.length - 1) {
    stepIndex.value = Math.max(0, STEPS.value.length - 1);
  }
});

function reset(): void {
  stepIndex.value = 0;
  revealing.value = false;
  accountId.value = null;
  localPath.value = null;
  localPathToken.value = null;
  driveFolderId.value = null;
  driveId.value = null;
  driveFolderPath.value = "";
  respectGitignore.value = true;
  includePatternsText.value = "";
  excludePatternsText.value = "";
  backupCloudOnly.value = false;
  encryptionEnabled.value = false;
  phraseConfirmed.value = false;
  phraseRevealed.value = false;
  recoveryPhrase.value = [];
  createdSource.value = null;
  pendingRecoveryAck.value = false;
  errorCode.value = null;
  revealErrorCode.value = null;
  submitting.value = false;
}

function close(): void {
  open.value = false;
}

async function chooseLocalFolder(): Promise<void> {
  errorCode.value = null;
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

/** Surface a Drive-picker failure on the wizard's shared error line. */
function onDrivePickerError(e: unknown): void {
  errorCode.value = toErrorCode(e);
}

/** Re-run the live preview under the current rules. The tree mounts (and starts
 * its first walk) when the exclusions step becomes active, so this only fires
 * for a rule CHANGE: a textarea blur, the gitignore toggle, or a "+"/"-" click
 * in the tree itself. */
function refreshPreview(): void {
  void previewTree.value?.restart();
}

/** A "+" in the tree: re-include that path. The glob is appended as a new line
 * to the include patterns (skipping an exact duplicate) and the tree
 * re-classifies. */
function onAppendInclude(pattern: string): void {
  includePatternsText.value = appendPatternLine(includePatternsText.value, pattern);
}

/** A "-" in the tree: exclude that path. */
function onAppendExclude(pattern: string): void {
  excludePatternsText.value = appendPatternLine(excludePatternsText.value, pattern);
}

function next(): void {
  if (stepIndex.value >= STEPS.value.length - 1) return;
  stepIndex.value += 1;
  // Each step lazily loads its own data as it becomes active: the Drive step
  // when the shared DriveFolderPicker mounts, the exclusions step when
  // ExclusionPreviewTree mounts and starts its first streaming walk.
}

function back(): void {
  if (stepIndex.value > 0) stepIndex.value -= 1;
}

async function confirm(): Promise<void> {
  if (
    accountId.value === null ||
    localPath.value === null ||
    localPathToken.value === null ||
    !destinationSettled.value
  ) {
    return;
  }
  submitting.value = true;
  errorCode.value = null;
  try {
    const displayName = localPath.value.split(/[\\/]/).filter(Boolean).pop();
    const result = await sources.add({
      accountId: accountId.value,
      displayName: displayName ?? localPath.value,
      localPathToken: localPathToken.value,
      localPath: localPath.value,
      // A destination that cannot be browsed contributed no id; its root is the
      // empty one, which is what the backend accepts for such a destination.
      driveFolderId: driveFolderId.value ?? "",
      driveId: driveId.value,
      driveFolderPath: driveFolderPath.value,
      encryptionEnabled: encryptionEnabled.value,
      respectGitignore: respectGitignore.value,
      includePatterns: includePatterns.value,
      excludePatterns: excludePatterns.value,
      placeholderPolicy: backupCloudOnly.value ? "force_download" : "skip",
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
    errorCode.value = toErrorCode(e);
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
  <div v-if="open" class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4">
    <div
      class="w-full max-w-lg space-y-4 rounded-lg border border-zinc-200 bg-white p-6 shadow-xl dark:border-zinc-800 dark:bg-zinc-900"
    >
      <h2 class="text-lg font-medium">
        {{ t("settings.addSource.title") }}
      </h2>

      <ol class="flex flex-wrap gap-2 text-xs">
        <li
          v-for="(s, i) in STEPS"
          :key="s"
          :class="
            i === stepIndex
              ? 'font-medium text-teal-700 dark:text-teal-300'
              : 'text-zinc-500 dark:text-zinc-400'
          "
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
          <select
            v-model="accountId"
            class="w-full"
            :class="inputCls"
            :disabled="accounts.accounts.length === 0"
          >
            <option v-if="accounts.accounts.length === 0" value="" disabled>
              {{ t("settings.addSource.noAccounts") }}
            </option>
            <option v-for="account in accounts.accounts" :key="account.id" :value="account.id">
              {{ account.email }}
            </option>
          </select>
        </label>

        <button type="button" :class="secondaryBtn" @click="chooseLocalFolder">
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

      <!-- Step 2: Drive folder picker (shared with the first-run setup wizard) -->
      <div v-else-if="step === 'driveFolder'" class="space-y-3">
        <DriveFolderPicker
          v-model:folder-id="driveFolderId"
          v-model:folder-path="driveFolderPath"
          v-model:drive-id="driveId"
          :account-id="accountId"
          :backend-kind="selectedAccount?.backendKind"
          @error="onDrivePickerError"
        />
      </div>

      <!-- Step 3: exclusions preview -->
      <div v-else-if="step === 'exclusions'" class="space-y-3">
        <label class="flex items-center gap-2 text-sm">
          <input
            v-model="respectGitignore"
            type="checkbox"
            class="accent-teal-600"
            @change="refreshPreview"
          />
          {{ t("settings.addSource.respectGitignoreLabel") }}
        </label>
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.addSource.includePatternsLabel")
          }}</span>
          <textarea
            v-model="includePatternsText"
            rows="2"
            class="w-full"
            :class="inputCls"
            @blur="refreshPreview"
          />
        </label>
        <!-- An include rule the scanner cannot bound to a fixed depth forces it
             into every excluded folder, so the walk stops being prunable. -->
        <div
          v-if="unconstrainedIncludes.length > 0"
          class="rounded-lg border border-amber-400 bg-amber-50 p-3 text-xs text-amber-800 dark:border-amber-700 dark:bg-amber-950/40 dark:text-amber-200"
          data-testid="include-pattern-warning"
          role="status"
        >
          <p class="font-medium">
            {{ t("settings.addSource.includeWarning.title") }}
          </p>
          <ul class="mt-1 list-disc space-y-0.5 pl-5 font-mono break-all">
            <li v-for="pattern in unconstrainedIncludes" :key="pattern">
              {{ pattern }}
            </li>
          </ul>
          <p class="mt-2">
            {{ t("settings.addSource.includeWarning.hint") }}
          </p>
        </div>
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.addSource.excludePatternsLabel")
          }}</span>
          <textarea
            v-model="excludePatternsText"
            rows="2"
            class="w-full"
            :class="inputCls"
            @blur="refreshPreview"
          />
        </label>

        <!-- Windows-only: OneDrive / cloud-only placeholder files exist only on
             Windows. Hiding it elsewhere does NOT change what the new source
             gets - `backupCloudOnly` stays false, which sends "skip", the same
             value a Windows user gets by leaving the box unticked and the same
             value the Rust `#[serde(default)]` would apply. -->
        <label v-if="showPlaceholderPolicy" class="flex items-start gap-2 text-sm">
          <input
            v-model="backupCloudOnly"
            type="checkbox"
            class="mt-0.5 accent-teal-600"
            data-testid="placeholder-policy-toggle"
          />
          <span>
            {{ t("settings.addSource.placeholderPolicyLabel") }}
            <span class="mt-0.5 block text-xs text-zinc-500 dark:text-zinc-400">
              {{ t("settings.addSource.placeholderPolicyCaption") }}
            </span>
          </span>
        </label>

        <!-- The live streaming tree: rows appear as the walk finds them, every
             folder starts collapsed, and each row's "+"/"-" appends the matching
             glob above and re-classifies. -->
        <ExclusionPreviewTree
          ref="previewTree"
          :local-path-token="localPathToken"
          :respect-gitignore="respectGitignore"
          :include-patterns="includePatterns"
          :exclude-patterns="excludePatterns"
          @append-include="onAppendInclude"
          @append-exclude="onAppendExclude"
        />
      </div>

      <!-- Step 4: encryption opt-in (phrase is revealed AFTER confirm, B3) -->
      <div v-else-if="step === 'encryption'" class="space-y-3">
        <label class="flex items-center gap-2 text-sm">
          <input v-model="encryptionEnabled" type="checkbox" class="accent-teal-600" />
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
        <!-- The destination: the folder picked on the destination step, or - when
             this account's destination cannot be browsed - the account's own
             fixed destination, so the summary is never a blank line. -->
        <p data-testid="confirm-destination">
          {{ t("settings.addSource.step.driveFolder") }}:
          {{ destinationIsBrowsable ? driveFolderPath : (selectedAccount?.email ?? "") }}
        </p>
        <p>
          {{ t("settings.sources.column.encryption") }}:
          {{ encryptionEnabled ? t("common.enabled") : t("common.disabled") }}
        </p>
      </div>

      <p v-if="errorCode" class="text-sm text-red-600" role="alert">
        {{ t(`errors.${errorCode}.long`) }}
      </p>

      <div class="flex justify-between gap-2">
        <button type="button" :class="secondaryBtn" @click="close">
          {{ t("common.cancel") }}
        </button>
        <div class="flex gap-2">
          <!-- B3 reveal step: a single "Done" button gated on acknowledgement;
               back/next are hidden so the phrase cannot be skipped. -->
          <button
            v-if="step === 'reveal'"
            type="button"
            :class="primaryBtn"
            :disabled="!phraseConfirmed || !phraseRevealed"
            data-testid="reveal-done"
            @click="finishReveal"
          >
            {{ t("common.done") }}
          </button>
          <template v-else>
            <button v-if="stepIndex > 0" type="button" :class="secondaryBtn" @click="back">
              {{ t("common.back") }}
            </button>
            <button
              v-if="step !== 'confirm'"
              type="button"
              :class="primaryBtn"
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
              :class="primaryBtn"
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
