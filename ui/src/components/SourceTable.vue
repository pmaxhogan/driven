<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import AddSourceWizard from "./AddSourceWizard.vue";
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
import { useToastsStore } from "../stores/toasts";
import { isWindows } from "../platform";
import type { BackendDto, SourceDto } from "../ipc/types";

// Sources settings tab body (SPEC s11.2; DESIGN s8.2). A table of sources with
// the per-row affordances the design calls for: enabled toggle, local path,
// Drive destination, account, encryption on/off, "Edit exclusions" (inline
// editor with a live preview), "Run now" (sync_now), and "Remove" (with an
// "also delete from Drive" opt-in). "Add source" opens the AddSourceWizard.
const { t, te } = useI18n();
const sources = useSourcesStore();
const accounts = useAccountsStore();
const toasts = useToastsStore();

// Shared design-system class strings (DRIVEN UI design system). Teal is the
// accent for primary affordances; red is destructive; amber is the warning
// accent for the data-safety recovery-phrase remediation action. Native controls
// carry explicit light/dark surfaces so they stay readable on a dark-theme OS.
const primaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const secondaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const destructiveBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-red-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-red-500 disabled:cursor-not-allowed disabled:opacity-50";
const warningBtn =
  "inline-flex items-center justify-center gap-2 rounded-md border border-amber-400 bg-amber-50 px-4 py-2 text-sm font-medium text-amber-800 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-500 disabled:cursor-not-allowed disabled:opacity-50 dark:border-amber-700 dark:bg-amber-950/40 dark:text-amber-200 dark:hover:bg-amber-900/40";
const inputCls =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-hidden focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const cardCls =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-xs dark:border-zinc-800 dark:bg-zinc-900";

// The OneDrive / cloud-only placeholder policy only DOES anything on Windows
// (its own caption said so), so it is not offered on other platforms. Evaluated
// once at setup: the host OS cannot change while the app is running.
const showPlaceholderPolicy = isWindows();

const wizard = ref<InstanceType<typeof AddSourceWizard> | null>(null);

// Inline exclusion-editor state, keyed by the source being edited.
const editingId = ref<string | null>(null);
const editRespectGitignore = ref(true);
const editIncludeText = ref("");
const editExcludeText = ref("");
// Issue #4: when true, back up OneDrive / cloud-only placeholder files
// (PlaceholderPolicy "force_download") instead of skipping them (the default).
const editBackupCloudOnly = ref(false);
// The live streaming folder-tree preview inside the open editor. It owns the
// walk (start / cancel / batched events); this component only tells it when the
// rules changed.
//
// The editor panel sits inside the per-source `v-for`, and Vue registers a
// template ref declared in a `v-for` SCOPE as an ARRAY (even though the `v-if`
// means at most one editor is ever mounted). So the ref is typed - and unwrapped
// below - as either shape; reading `.restart` off the raw ref would silently be
// `undefined` and every rule edit would stop re-classifying.
type PreviewTreeRef = InstanceType<typeof ExclusionPreviewTree>;
const editPreviewTree = ref<PreviewTreeRef | PreviewTreeRef[] | null>(null);

/** The single mounted preview tree, whichever shape Vue stored the ref in. */
function currentPreviewTree(): PreviewTreeRef | null {
  const found = editPreviewTree.value;
  if (Array.isArray(found)) return found[0] ?? null;
  return found;
}
const savingEdit = ref(false);

// Inline remove-confirmation state.
const confirmingRemoveId = ref<string | null>(null);
const deleteRemote = ref(false);
const removing = ref(false);
// Issue #227: a "delete the backed-up files too" removal can genuinely fail
// (destination unreachable, permission revoked, ...) - the stable SPEC s24
// CODE (localized via t(`errors.${code}.long`)), same contract as the
// versioning / reveal panels' inline errors. `remove_source` aborts BEFORE
// touching anything local on any such failure, so the source stays listed
// and the confirm panel stays open for a retry.
const removeErrorCode = ref<string | null>(null);

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

const accountEmailById = computed<Record<string, string>>(() => {
  const map: Record<string, string> = {};
  for (const account of accounts.accounts) {
    map[account.id] = account.email;
  }
  return map;
});

/**
 * What happens to superseded versions AT THIS SOURCE'S DESTINATION.
 *
 * This genuinely differs per backend rather than being a wording choice, so a
 * single sentence could only ever be wrong somewhere: Drive keeps a superseded
 * object in its trash (recoverable for ~30 days), an S3 or local-folder
 * destination keeps it in a `.driven-versions` area Driven owns there (pruned to
 * the per-source limit, and NOT a recovery path for deletions), and SFTP keeps
 * nothing at all. The versioning panel used to state Drive's behaviour
 * unconditionally, directly contradicting the S3 setup screen's own "S3 has no
 * trash" warning inside the same app.
 *
 * Unseeded / unknown backends fall back to the neutral `default`, because
 * `BackendKind::ALL` is Rust-owned and gains entries ahead of the locale file.
 */
function retentionNote(accountId: string): string {
  const kind = accounts.accounts.find((a) => a.id === accountId)?.backendKind;
  const key = `versionRetention.${kind}`;
  return kind !== undefined && te(key) ? t(key) : t("versionRetention.default");
}

/**
 * Issue #220: the destination CAPABILITY descriptors, joined to a source by its
 * account's `backendKind`. Fetched rather than hardcoded, following #219: the set
 * of backends and what each can do is Rust-owned (`driven_backend::descriptors()`),
 * so a UI-side list of which backends can version would silently rot the moment
 * one gains or loses the ability - as S3 and the local folder did when #220 part 2
 * gave them a real version store.
 */
const backends = ref<BackendDto[]>([]);
async function loadBackends(): Promise<void> {
  try {
    const list = await ipc.listBackends();
    backends.value = Array.isArray(list) ? list : [];
  } catch {
    // A descriptor-fetch failure must not brick the sources table. See
    // `keepsVersions` for why falling back to permissive is safe here.
    backends.value = [];
  }
}

/**
 * Whether this source's DESTINATION can really keep previous versions.
 *
 * A destination that cannot put the superseded bytes under an object of their own
 * re-uploads over the previous copy, so a retained version would point at the
 * CURRENT bytes and "restore as of an earlier date" would return today's content
 * while reporting success (issue #220). Since part 2 that is SFTP alone - Drive,
 * S3 and the local folder all keep the old bytes - but the answer is read from the
 * descriptors, never assumed. The editor is not offered where it is false.
 *
 * An UNKNOWN or not-yet-loaded backend resolves PERMISSIVE, matching the house
 * rule from #219 (`setup.ts` / `AddSourceWizard.vue` both `?? true`) so a
 * descriptor hiccup never hides a control a Drive source legitimately needs. That
 * is safe because `set_source_versioning` enforces the same rule server-side: the
 * worst a permissive default can do is render an affordance whose Save fails
 * loudly, never store a promise the destination cannot keep.
 */
function keepsVersions(accountId: string): boolean {
  const kind = accounts.accounts.find((a) => a.id === accountId)?.backendKind;
  if (kind === undefined) return true;
  return backends.value.find((b) => b.id === kind)?.supportsVersionHistory ?? true;
}

onMounted(async () => {
  await Promise.all([sources.refresh(), accounts.refresh(), loadBackends()]);
});

// The include rules in the open editor that stop the scanner from pruning
// excluded folders, recomputed AS THE USER TYPES (the preview walk itself only
// re-runs on blur, but the guidance should not wait for it).
const unconstrainedIncludes = computed(() => unconstrainedIncludePatterns(editIncludeText.value));

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
  editBackupCloudOnly.value = source.placeholderPolicy === "force_download";
  // The tree starts its first streaming walk when it mounts with the panel.
}

function cancelEdit(): void {
  // Unmounting the tree cancels the walk it has in flight, so closing the editor
  // over a huge source does not leave a scan burning CPU.
  editingId.value = null;
}

/** Re-run the live preview under the current rules (a textarea blur, the
 *  gitignore toggle, or a "+"/"-" click in the tree itself). */
function refreshEditPreview(): void {
  void currentPreviewTree()?.restart();
}

/** A "+" in the tree: re-include that path. The glob is appended as a new line
 * to the include patterns (skipping an exact duplicate); the tree re-classifies
 * and Save persists the patterns as edited. */
function onAppendInclude(pattern: string): void {
  editIncludeText.value = appendPatternLine(editIncludeText.value, pattern);
}

/** A "-" in the tree: exclude that path. */
function onAppendExclude(pattern: string): void {
  editExcludeText.value = appendPatternLine(editExcludeText.value, pattern);
}

async function saveEdit(source: SourceDto): Promise<void> {
  savingEdit.value = true;
  try {
    await sources.update(source.id, {
      respectGitignore: editRespectGitignore.value,
      includePatterns: splitPatterns(editIncludeText.value),
      excludePatterns: splitPatterns(editExcludeText.value),
      placeholderPolicy: editBackupCloudOnly.value ? "force_download" : "skip",
    });
    editingId.value = null;
    // Saving closes the editor, so without this the only feedback that the new
    // patterns took is the panel disappearing.
    toasts.push({ kind: "success", message: t("toast.rulesSaved") });
  } finally {
    savingEdit.value = false;
  }
}

// Issue #36: per-source point-in-time versioning panel state. The config lives
// in the settings KV (not a SourceRow field), so it is loaded / saved via its own
// IPC (getSourceVersioning / setSourceVersioning), not the sources store patch.
const versioningId = ref<string | null>(null);
const versioningEnabled = ref(false);
const versioningCap = ref(10);
const versioningLoading = ref(false);
const savingVersioning = ref(false);
// The versioning-load error as a stable SPEC s24 CODE (localized in the template
// via t(`errors.${code}.long`)), same contract as the recovery reveal error.
const versioningErrorCode = ref<string | null>(null);

async function beginVersioning(source: SourceDto): Promise<void> {
  editingId.value = null;
  confirmingRemoveId.value = null;
  versioningErrorCode.value = null;
  versioningId.value = source.id;
  versioningLoading.value = true;
  try {
    const cfg = await ipc.getSourceVersioning(source.id);
    versioningEnabled.value = cfg.enabled;
    versioningCap.value = cfg.countCap;
  } catch (e) {
    // The load failed: DO NOT render the editor over the PREVIOUS panel's stale
    // enabled/cap (Save would persist those to THIS source). Surface the error in
    // place of the inputs; the template hides the inputs + Save while it is set.
    versioningErrorCode.value = toErrorCode(e);
  } finally {
    versioningLoading.value = false;
  }
}

function cancelVersioning(): void {
  versioningId.value = null;
  versioningErrorCode.value = null;
}

async function saveVersioning(source: SourceDto): Promise<void> {
  savingVersioning.value = true;
  try {
    // Preserve the existing size guard (maxBytes) rather than resetting it - the
    // panel only edits enabled + the count cap. The backend clamps countCap.
    const current = await ipc.getSourceVersioning(source.id);
    await ipc.setSourceVersioning(source.id, {
      enabled: versioningEnabled.value,
      countCap: Math.max(1, Math.round(versioningCap.value)),
      maxBytes: current.maxBytes,
    });
    versioningId.value = null;
  } finally {
    savingVersioning.value = false;
  }
}

/**
 * Issue #220: clear a STALE versioning flag on a destination that cannot honour
 * it - a source switched on before this gate existed (or by an older build).
 *
 * This is the remedy half of the gate. Enabling is refused both here and in
 * `set_source_versioning`, but DISABLING stays open on every destination: without
 * it such a source would be stuck advertising a point-in-time capability nothing
 * can deliver, with no way to put it right.
 */
async function disableVersioning(source: SourceDto): Promise<void> {
  savingVersioning.value = true;
  try {
    const current = await ipc.getSourceVersioning(source.id);
    await ipc.setSourceVersioning(source.id, {
      enabled: false,
      countCap: current.countCap,
      maxBytes: current.maxBytes,
    });
    versioningEnabled.value = false;
    versioningId.value = null;
  } catch (e) {
    // Unlike Save (whose worst case is "my cap did not stick"), a silent failure
    // here would leave the user believing they cleared a setting that still
    // claims a point-in-time restore. Surface the stable code - same contract as
    // the recovery-ack error - so the panel says the flag is still on. Re-opening
    // the panel reloads the real state and offers the remedy again.
    versioningErrorCode.value = toErrorCode(e);
  } finally {
    savingVersioning.value = false;
  }
}

function beginRemove(sourceId: string): void {
  confirmingRemoveId.value = sourceId;
  deleteRemote.value = false;
  removeErrorCode.value = null;
}

function cancelRemove(): void {
  confirmingRemoveId.value = null;
  deleteRemote.value = false;
  removeErrorCode.value = null;
}

async function confirmRemove(sourceId: string): Promise<void> {
  removeErrorCode.value = null;
  removing.value = true;
  try {
    await sources.remove(sourceId, deleteRemote.value);
    confirmingRemoveId.value = null;
    deleteRemote.value = false;
  } catch (e) {
    // Issue #227: `remove_source` aborts with nothing removed on a failed
    // remote deletion (destination unreachable, permission revoked, ...).
    // Keep the confirm panel open with the checkbox still ticked so the
    // error is visible and a retry is one click away, rather than silently
    // leaving the source in the list with no explanation.
    removeErrorCode.value = toErrorCode(e);
  } finally {
    removing.value = false;
  }
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
            <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
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
              class="rounded-sm bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-800 dark:bg-amber-950/50 dark:text-amber-300"
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
          <button
            type="button"
            :class="secondaryBtn"
            data-testid="versioning-button"
            @click="beginVersioning(source)"
          >
            {{ t("settings.sources.versioningButton") }}
          </button>
          <button type="button" :class="secondaryBtn" @click="beginRemove(source.id)">
            {{ t("settings.sources.removeButton") }}
          </button>
        </div>

        <!-- Issue #36: per-source point-in-time versioning editor. -->
        <div
          v-if="versioningId === source.id"
          class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-700"
          data-testid="versioning-editor"
        >
          <!-- Issue #220: the intro PROMISES point-in-time restore, so it is only
               honest where the destination can actually keep previous versions.
               Elsewhere it is replaced by the plain statement that it cannot. -->
          <p
            v-if="!keepsVersions(source.accountId)"
            class="text-sm text-zinc-600 dark:text-zinc-400"
            data-testid="versioning-unsupported"
          >
            {{ t("settings.sources.versioning.unsupported") }}
          </p>
          <p v-else class="text-sm text-zinc-600 dark:text-zinc-400">
            {{ t("settings.sources.versioning.intro") }}
          </p>
          <!-- What the DESTINATION does with superseded versions - a per-backend
               fact, not a wording choice. See retentionNote(). For a destination
               that keeps none this is also the WHY, and for S3 it carries the
               remedy (the provider's own bucket versioning). -->
          <p class="text-sm text-zinc-600 dark:text-zinc-400" data-testid="versioning-retention">
            {{ retentionNote(source.accountId) }}
          </p>
          <p v-if="versioningLoading" class="text-sm text-zinc-500">
            {{ t("common.loading") }}
          </p>
          <p
            v-else-if="versioningErrorCode"
            class="text-sm text-red-600"
            data-testid="versioning-error"
          >
            {{ t(`errors.${versioningErrorCode}.long`) }}
          </p>
          <!-- Issue #220: the editor renders ONLY where the destination can
               honour it. Elsewhere there is deliberately no control at all - not
               even a disabled one - because a control for something that would
               silently not work is the promise this gate exists to remove. -->
          <template v-else-if="keepsVersions(source.accountId)">
            <label class="flex items-center gap-2 text-sm">
              <input
                v-model="versioningEnabled"
                type="checkbox"
                class="accent-teal-600"
                data-testid="versioning-enabled"
              />
              {{ t("settings.sources.versioning.enabledLabel") }}
            </label>
            <label class="flex items-center gap-2 text-sm">
              <span class="text-zinc-600 dark:text-zinc-400">{{
                t("settings.sources.versioning.capLabel")
              }}</span>
              <input
                v-model.number="versioningCap"
                type="number"
                min="1"
                max="1000"
                class="w-24"
                :class="inputCls"
                :disabled="!versioningEnabled"
                data-testid="versioning-cap"
              />
            </label>
          </template>
          <!-- A source whose flag was already set (before this gate existed, or by
               an older build) gets the one thing it still needs: the stale setting
               called out, and the remedy button below to turn it back off. -->
          <p
            v-else-if="versioningEnabled"
            class="rounded-md border border-amber-400 bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:border-amber-700 dark:bg-amber-950/40 dark:text-amber-200"
            data-testid="versioning-stale"
          >
            {{ t("settings.sources.versioning.staleEnabled") }}
          </p>
          <div class="flex gap-2">
            <button
              v-if="!versioningErrorCode && keepsVersions(source.accountId)"
              type="button"
              :class="primaryBtn"
              :disabled="savingVersioning || versioningLoading"
              data-testid="versioning-save"
              @click="saveVersioning(source)"
            >
              {{ t("common.save") }}
            </button>
            <!-- The remedy for a stale flag on a destination that cannot honour it.
                 Offered ONLY in that state: there is nothing else to save here. -->
            <button
              v-if="
                !versioningErrorCode &&
                !versioningLoading &&
                !keepsVersions(source.accountId) &&
                versioningEnabled
              "
              type="button"
              :class="warningBtn"
              :disabled="savingVersioning"
              data-testid="versioning-disable"
              @click="disableVersioning(source)"
            >
              {{ t("settings.sources.versioning.disableButton") }}
            </button>
            <button type="button" :class="secondaryBtn" @click="cancelVersioning">
              {{ t("common.cancel") }}
            </button>
          </div>
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
              @change="refreshEditPreview"
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
              @blur="refreshEditPreview"
            />
          </label>
          <!-- An include rule the scanner cannot bound to a fixed depth forces
               it into every excluded folder, so the walk stops being prunable. -->
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
              v-model="editExcludeText"
              rows="2"
              class="w-full"
              :class="inputCls"
              @blur="refreshEditPreview"
            />
          </label>
          <!-- Windows-only: this policy governs OneDrive / cloud-only placeholder
               files, which exist only on Windows. Hiding it elsewhere does NOT
               change what a source gets: the toggle is SEEDED from the source's
               stored policy and written back unchanged, so a value a Windows user
               set survives an edit made on another platform. -->
          <label v-if="showPlaceholderPolicy" class="flex items-start gap-2 text-sm">
            <input
              v-model="editBackupCloudOnly"
              type="checkbox"
              class="mt-0.5 accent-teal-600"
              data-testid="placeholder-policy-toggle"
            />
            <span>
              {{ t("settings.sources.placeholderPolicyLabel") }}
              <span class="mt-0.5 block text-xs text-zinc-500 dark:text-zinc-400">
                {{ t("settings.sources.placeholderPolicyCaption") }}
              </span>
            </span>
          </label>
          <!-- The live streaming tree: rows appear as the walk finds them, every
               folder starts collapsed, and each row's "+"/"-" appends the
               matching glob above and re-classifies. -->
          <ExclusionPreviewTree
            ref="editPreviewTree"
            :source-id="source.id"
            :respect-gitignore="editRespectGitignore"
            :include-patterns="splitPatterns(editIncludeText)"
            :exclude-patterns="splitPatterns(editExcludeText)"
            @append-include="onAppendInclude"
            @append-exclude="onAppendExclude"
          />
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
          <p
            v-if="removeErrorCode"
            class="text-red-700 dark:text-red-400"
            data-testid="remove-error"
          >
            {{ t(`errors.${removeErrorCode}.long`) }}
          </p>
          <div class="flex gap-2">
            <button
              type="button"
              :class="destructiveBtn"
              :disabled="removing"
              data-testid="source-remove-confirm-button"
              @click="confirmRemove(source.id)"
            >
              {{ t("settings.sources.removeButton") }}
            </button>
            <button type="button" :class="secondaryBtn" :disabled="removing" @click="cancelRemove">
              {{ t("common.cancel") }}
            </button>
          </div>
        </div>
      </li>
    </ul>

    <AddSourceWizard ref="wizard" @created="sources.refresh()" />
  </div>
</template>
