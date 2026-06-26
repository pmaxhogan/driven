<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import AddSourceWizard from "./AddSourceWizard.vue";
import RecoveryPhraseReveal from "./RecoveryPhraseReveal.vue";
import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
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

// Shared design-system class strings (DRIVEN UI design system). Teal is the
// accent for primary affordances; red is destructive; amber is the warning
// accent for the data-safety recovery-phrase remediation action. Native controls
// carry explicit light/dark surfaces so they stay readable on a dark-theme OS.
const primaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-teal-600 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const secondaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const destructiveBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-red-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-red-500 disabled:cursor-not-allowed disabled:opacity-50";
const warningBtn =
  "inline-flex items-center justify-center gap-2 rounded-md border border-amber-400 bg-amber-50 px-4 py-2 text-sm font-medium text-amber-800 transition-colors hover:bg-amber-100 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-500 disabled:cursor-not-allowed disabled:opacity-50 dark:border-amber-700 dark:bg-amber-950/40 dark:text-amber-200 dark:hover:bg-amber-900/40";
const inputCls =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-none focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const cardCls =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";

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

// R5-P1-2 (DATA-SAFETY): post-restart recovery-phrase reveal/ack state, keyed by
// the pending source being remediated. The wizard's reveal/ack flow lives only in
// volatile wizard state, so a first-encrypted source that survived a crash/restart
// (durably pending) needs its OWN reachable reveal/ack action here. Opening the
// panel fetches + records the backend reveal (revealRecoveryPhrase), shows the 24
// words via RecoveryPhraseReveal, and gates ack on the user attesting they saved
// them; ack (ackRecoveryPhrase) enables the source + clears the pending state.
const revealingId = ref<string | null>(null);
const revealPhrase = ref<string[]>([]);
const revealConfirmed = ref(false);
const revealEverShown = ref(false);
const revealAcking = ref(false);
// R8-P2-1: the recovery reveal/ack error as a stable SPEC s24 CODE (not a raw
// String(e), which renders a Tauri structured error as `[object Object]` and can
// leak backend English). The template localizes it via t(`errors.${code}.long`).
const revealErrorCode = ref<string | null>(null);

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
  // R4-P1-2 (DATA-SAFETY): a first-encrypted source still awaiting its
  // recovery-phrase ack cannot be enabled here - the user must finish the
  // reveal+ack step first. The toggle is disabled in the template, but guard the
  // handler too so a programmatic change cannot bypass it (the backend
  // update_source is the real guard and would reject it regardless).
  if (source.pendingRecoveryAck) {
    return;
  }
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

// R5-P1-2 / R7-P2-1 (DATA-SAFETY): open the post-restart reveal/ack panel for a
// pending first-encrypted source. Opening the panel must NOT record a backend
// reveal - the durable `revealed=1` state may only be set once the user actually
// clicks Reveal. So this only resets state + opens the panel; the actual
// revealRecoveryPhrase IPC happens in `revealPhraseAction` (threaded into
// RecoveryPhraseReveal as its reveal-action, fired on the Reveal click). Any
// other inline panel (edit / remove) is closed so only one is open at a time.
function beginRevealAck(source: SourceDto): void {
  editingId.value = null;
  confirmingRemoveId.value = null;
  revealErrorCode.value = null;
  revealConfirmed.value = false;
  revealEverShown.value = false;
  revealPhrase.value = [];
  revealingId.value = source.id;
}

// R7-P2-1: the reveal action threaded into RecoveryPhraseReveal - the BACKEND
// reveal the ack gate depends on. Fired only when the user clicks Reveal. It
// fetches + durably records the reveal and stores the 24 words for display; if it
// rejects, RecoveryPhraseReveal surfaces the error and leaves the phrase hidden +
// the ack locked, and the backend reveal is never recorded.
//
// R9-P1-2: RETURN the revealed phrase so RecoveryPhraseReveal latches the reveal
// from the return value directly. The `revealPhrase` prop is still set (for
// display), but it lands on a later reactive tick; returning the words lets the
// ack control unlock deterministically without waiting for that prop delivery.
async function revealPhraseAction(): Promise<string[]> {
  const id = revealingId.value;
  if (id === null) return [];
  const phrase = await sources.revealRecoveryPhrase(id);
  revealPhrase.value = phrase;
  return phrase;
}

// R7-P2-1 / R8-P2-1: surface a backend reveal error from RecoveryPhraseReveal as
// a stable SPEC s24 code (normalized via toErrorCode), so the template localizes
// it - never `[object Object]` / leaked backend English.
function onRevealError(code: unknown): void {
  revealErrorCode.value = toErrorCode(code);
}

function cancelRevealAck(): void {
  revealingId.value = null;
  revealPhrase.value = [];
  revealConfirmed.value = false;
  revealEverShown.value = false;
  revealErrorCode.value = null;
}

// RecoveryPhraseReveal signals when the phrase has actually been shown (so the ack
// checkbox unlocks) or re-locked (clears the acknowledgement).
function onRevealShown(value: boolean): void {
  revealEverShown.value = value;
  if (!value) revealConfirmed.value = false;
}

// R5-P1-2: acknowledge the saved phrase, ENABLING the until-now-disabled source.
// The backend rejects the ack unless a real reveal was recorded (done by
// beginRevealAck), so the client gate is backed by the server gate. On success the
// list refreshes (the source is now enabled, no longer pending) and the panel closes.
async function confirmRevealAck(sourceId: string): Promise<void> {
  if (!revealConfirmed.value || !revealEverShown.value) return;
  revealAcking.value = true;
  revealErrorCode.value = null;
  try {
    await sources.ackRecoveryPhrase(sourceId);
    cancelRevealAck();
  } catch (e) {
    // R8-P2-1: store the stable code; the template localizes it.
    revealErrorCode.value = toErrorCode(e);
  } finally {
    revealAcking.value = false;
  }
}
</script>

<template>
  <div class="space-y-3">
    <div class="flex items-center justify-between">
      <h2 class="text-lg font-medium">
        {{ t("settings.sources.title") }}
      </h2>
      <button
        v-if="sources.sources.length > 0"
        type="button"
        :class="primaryBtn"
        @click="openWizard"
      >
        {{ t("settings.sources.addButton") }}
      </button>
    </div>

    <p v-if="sources.loading" class="text-sm text-zinc-500">
      {{ t("common.loading") }}
    </p>
    <p v-else-if="sources.error" class="text-sm text-red-600">
      {{ sources.error }}
    </p>
    <div
      v-else-if="sources.sources.length === 0"
      class="rounded-lg border border-dashed border-zinc-300 p-8 text-center dark:border-zinc-700"
      data-testid="sources-empty"
    >
      <p class="text-sm font-medium text-zinc-600 dark:text-zinc-300">
        {{ t("settings.sources.emptyTitle") }}
      </p>
      <p class="mt-1 text-sm text-zinc-500">
        {{ t("settings.sources.emptyHint") }}
      </p>
      <button
        type="button"
        class="mt-4"
        :class="primaryBtn"
        data-testid="sources-empty-add"
        @click="openWizard"
      >
        {{ t("settings.sources.addButton") }}
      </button>
    </div>
    <ul v-else class="space-y-3">
      <li v-for="source in sources.sources" :key="source.id" class="space-y-3" :class="cardCls">
        <div class="flex items-start justify-between gap-3">
          <div class="min-w-0 space-y-2">
            <p class="text-sm font-medium">
              {{ source.displayName }}
            </p>
            <dl class="grid grid-cols-[auto,1fr] gap-x-3 gap-y-1 text-xs">
              <dt class="text-zinc-500">{{ t("settings.sources.column.localPath") }}</dt>
              <dd class="break-all text-zinc-700 dark:text-zinc-300">{{ source.localPath }}</dd>
              <dt class="text-zinc-500">{{ t("settings.sources.column.driveDestination") }}</dt>
              <dd class="break-all text-zinc-700 dark:text-zinc-300">
                {{ source.driveFolderPath }}
              </dd>
              <dt class="text-zinc-500">{{ t("settings.sources.column.account") }}</dt>
              <dd class="break-all text-zinc-700 dark:text-zinc-300">
                {{ accountEmailById[source.accountId] ?? source.accountId }}
              </dd>
              <dt class="text-zinc-500">{{ t("settings.sources.column.encryption") }}</dt>
              <dd class="text-zinc-700 dark:text-zinc-300">
                {{ source.encryptionEnabled ? t("common.yes") : t("common.no") }}
              </dd>
            </dl>
          </div>
          <div class="flex shrink-0 flex-col items-end gap-1">
            <label class="flex items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
              {{ t("settings.sources.column.enabled") }}
              <input
                type="checkbox"
                class="accent-teal-600"
                :checked="source.enabled"
                :disabled="source.pendingRecoveryAck"
                :aria-label="t('settings.sources.column.enabled')"
                :title="
                  source.pendingRecoveryAck
                    ? t('settings.sources.pendingRecoveryAckTooltip')
                    : undefined
                "
                @change="toggleEnabled(source)"
              />
            </label>
            <span
              v-if="source.pendingRecoveryAck"
              class="rounded bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-800 dark:bg-amber-950/50 dark:text-amber-300"
              data-testid="pending-recovery-ack-badge"
            >
              {{ t("settings.sources.pendingRecoveryAckBadge") }}
            </span>
          </div>
        </div>

        <div class="flex flex-wrap gap-2">
          <button
            v-if="source.pendingRecoveryAck"
            type="button"
            :class="warningBtn"
            data-testid="reveal-ack-button"
            @click="beginRevealAck(source)"
          >
            {{ t("settings.sources.revealAckButton") }}
          </button>
          <button type="button" :class="secondaryBtn" @click="beginEditExclusions(source)">
            {{ t("settings.sources.editExclusionsButton") }}
          </button>
          <button type="button" :class="secondaryBtn" @click="runNow(source)">
            {{ t("settings.sources.runNowButton") }}
          </button>
          <button type="button" :class="secondaryBtn" @click="beginRemove(source.id)">
            {{ t("settings.sources.removeButton") }}
          </button>
        </div>

        <div
          v-if="editingId === source.id"
          class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-700"
          data-testid="exclusion-editor"
        >
          <label class="flex items-center gap-2 text-sm">
            <input
              v-model="editRespectGitignore"
              type="checkbox"
              class="accent-teal-600"
              @change="loadEditPreview(source)"
            />
            {{ t("settings.addSource.respectGitignoreLabel") }}
          </label>
          <label class="block space-y-1 text-sm">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.addSource.includePatternsLabel")
            }}</span>
            <textarea
              v-model="editIncludeText"
              rows="2"
              class="w-full"
              :class="inputCls"
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
              class="w-full"
              :class="inputCls"
              @blur="loadEditPreview(source)"
            />
          </label>
          <p v-if="editPreviewLoading" class="text-sm text-zinc-500">
            {{ t("common.loading") }}
          </p>
          <p v-else-if="editPreview" class="text-sm">
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
              :class="primaryBtn"
              :disabled="savingEdit"
              @click="saveEdit(source)"
            >
              {{ t("common.save") }}
            </button>
            <button type="button" :class="secondaryBtn" @click="cancelEdit">
              {{ t("common.cancel") }}
            </button>
          </div>
        </div>

        <div
          v-if="revealingId === source.id"
          class="space-y-2 rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/30"
          data-testid="reveal-ack-panel"
        >
          <p class="text-amber-700 dark:text-amber-300">
            {{ t("settings.sources.revealAckIntro") }}
          </p>
          <RecoveryPhraseReveal
            v-model:confirmed="revealConfirmed"
            :phrase="revealPhrase"
            :reveal-action="revealPhraseAction"
            @update:revealed="onRevealShown"
            @reveal-error="onRevealError"
          />
          <p v-if="revealErrorCode" class="text-red-600">
            {{ t(`errors.${revealErrorCode}.long`) }}
          </p>
          <div class="flex gap-2">
            <button
              type="button"
              :class="primaryBtn"
              :disabled="!revealConfirmed || !revealEverShown || revealAcking"
              data-testid="reveal-ack-confirm"
              @click="confirmRevealAck(source.id)"
            >
              {{ t("settings.sources.revealAckConfirmButton") }}
            </button>
            <button type="button" :class="secondaryBtn" @click="cancelRevealAck">
              {{ t("common.cancel") }}
            </button>
          </div>
        </div>

        <div
          v-if="confirmingRemoveId === source.id"
          class="space-y-2 rounded-lg border border-red-300 bg-red-50 p-3 text-sm dark:border-red-800 dark:bg-red-950/30"
          data-testid="source-remove-confirm"
        >
          <p
            v-if="source.pendingRecoveryAck"
            class="text-red-700 dark:text-red-400"
            data-testid="pending-remove-warning"
          >
            {{ t("settings.sources.pendingRemoveWarning") }}
          </p>
          <label class="flex items-center gap-2">
            <input v-model="deleteRemote" type="checkbox" class="accent-teal-600" />
            {{ t("settings.sources.deleteRemoteLabel") }}
          </label>
          <div class="flex gap-2">
            <button type="button" :class="destructiveBtn" @click="confirmRemove(source.id)">
              {{ t("settings.sources.removeButton") }}
            </button>
            <button type="button" :class="secondaryBtn" @click="cancelRemove">
              {{ t("common.cancel") }}
            </button>
          </div>
        </div>
      </li>
    </ul>

    <AddSourceWizard ref="wizard" @created="sources.refresh()" />
  </div>
</template>
