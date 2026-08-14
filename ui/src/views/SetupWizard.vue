<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";

import BackendPicker from "../components/BackendPicker.vue";
import CredentialsWalkthrough from "../components/CredentialsWalkthrough.vue";
import S3CredentialsForm from "../components/S3CredentialsForm.vue";
import SshCredentialsForm from "../components/SshCredentialsForm.vue";
import DriveFolderPicker from "../components/DriveFolderPicker.vue";
import LocalFolderForm from "../components/LocalFolderForm.vue";
import RecoveryPhraseReveal from "../components/RecoveryPhraseReveal.vue";
import { pickFolderDialog } from "../ipc/commands";
import { toErrorMessage } from "../ipc/errors";
import { useSetupStore, WIZARD_STEPS } from "../stores/setup";
import type {
  CreateLocalFolderAccountRequest,
  CreateS3AccountRequest,
  CreateSftpAccountRequest,
} from "../ipc/types";
import { LOCAL_FOLDER_BACKEND_ID, S3_BACKEND_ID, SFTP_BACKEND_ID } from "../ipc/types";

// Setup wizard (SPEC s25 /setup; DESIGN s8.5 5-step wizard). Drives the whole
// first-run flow as a stepper:
//   1 welcome           - what Driven is
//   2 credentials       - BYO OAuth client paste + loopback sign-in (SPEC s6.1)
//   3 source            - pick first local folder + Drive destination
//   4 encryption        - opt-in + one-time recovery-phrase reveal
//   5 confirm           - start the initial sync
// The OAuth IPC sequence (begin -> submitCredentials -> startSignin -> open auth
// URL -> poll / oauth:complete -> finish) lives in CredentialsWalkthrough + the
// setup store. The source is created with its encryption flag when leaving the
// encryption step, then synced from the confirm step.
//
// i18n: every visible string flows through t() against seeded keys (DESIGN s8.7).
// IPC path safety (SPEC s11.6.1 / C1): the local folder is chosen via the
// BACKEND-owned native folder dialog (pickFolderDialog), which returns a one-shot
// token bound to the chosen path; add_source receives that token (never a
// webview-supplied string), so the backend can prove the path is dialog-derived.
// B3: the source is created on leaving the encryption step, and any recovery
// phrase it returns is revealed on the confirm step + Finish is gated on the
// user acknowledging they saved it (never an empty / un-acknowledged phrase).

const { t } = useI18n();
const router = useRouter();
const setup = useSetupStore();

const total = WIZARD_STEPS.length;
const current = computed(() => setup.stepIndex + 1);

const pickingFolder = ref(false);

// Design-system class strings (shared verbatim across slices for consistency):
// teal primary CTAs (Next / Finish / Start backup), zinc secondary (Back / file
// pickers), card panels, and teal focus rings - all readable in dark mode.
const PRIMARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const CARD =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-xs dark:border-zinc-800 dark:bg-zinc-900";

// Load the destinations this build can construct so the welcome step's picker
// has options. The wizard SESSION is deliberately NOT begun here: the session is
// stamped with the chosen destination server-side, so it must not open until the
// user has left the picker. `connectAccount` begins one on demand if the
// credentials step is somehow reached without it, so nothing can dead-end.
onMounted(async () => {
  setup.reset();
  await setup.loadBackends();
});

// R4-P2-4: if the wizard is left while an OAuth session is still open (the user
// navigated away mid-flow), tell the backend to drop it so its BYO creds +
// tokens do not linger in the server-side registry. After a successful finish
// the session is already consumed, so this cancel is an idempotent no-op.
onBeforeUnmount(() => {
  if (setup.session) {
    void setup.cancel();
  }
});

const errorLong = computed<string | null>(() =>
  setup.errorCode ? t(`errors.${setup.errorCode}.long`) : null
);

/** The technical-detail line under the localized error: stable code, plus the
 * backend's redacted message when one accompanied the failure. Composed here so
 * the template carries no raw string literals (i18n no-raw-text) - the line is
 * intentionally untranslated diagnostic text. */
const errorDetailLine = computed<string | null>(() =>
  setup.errorCode
    ? setup.errorDetail
      ? `${setup.errorCode} - ${setup.errorDetail}`
      : setup.errorCode
    : null
);

/** Step 2's heading, which names the destination being connected.
 *
 * Explicit per-backend branches (rather than a two-way OAuth/not-OAuth split
 * that defaulted anything non-OAuth to the S3 title): with three non-OAuth
 * destinations now offered (local folder, S3, SSH/SFTP), "which title" is a
 * per-backend question, and a catch-all silently mislabels whichever one it
 * was not written for - which is exactly what happened here before SFTP
 * existed to expose it. */
const credentialsStepTitle = computed<string>(() => {
  if (setup.backendUsesOauth) return t("wizard.step2.title");
  if (setup.backendId === LOCAL_FOLDER_BACKEND_ID) return t("localFolderSetup.title");
  if (setup.backendId === S3_BACKEND_ID) return t("s3Setup.title");
  if (setup.backendId === SFTP_BACKEND_ID) return t("sftpSetup.title");
  // A destination this build's picker offers but that has no dedicated
  // credentials form (should not happen - `list_backends` and this wizard
  // ship together - but a neutral title beats a wrong one).
  return t("wizard.step2.unsupportedTitle");
});

// --- Per-step "can advance" gating -------------------------------------------

const canAdvance = computed(() => {
  switch (setup.step) {
    case "welcome":
      return true;
    case "credentials":
      // Advancing is automatic on sign-in (CredentialsWalkthrough @complete),
      // but also allow Next once signed in.
      return setup.signedIn;
    case "source":
      // `destinationSelected` (store-owned) rather than `!!setup.driveFolderId`:
      // the empty string is a REAL destination (an S3 bucket root), and
      // truthiness rejected it - which is what left Next permanently disabled for
      // an S3 account. The predicate lives in the store so this view and
      // `createFirstSource` cannot drift apart again.
      return !!setup.localPathToken && setup.destinationSelected;
    case "encryption":
      return !setup.busy;
    case "confirm":
      return false; // terminal step uses Finish, not Next.
    default:
      return false;
  }
});

// --- Step 3: source pickers --------------------------------------------------

async function chooseLocalFolder(): Promise<void> {
  pickingFolder.value = true;
  setup.clearError();
  try {
    // C1: the BACKEND owns the dialog and returns { path, token }. We store both
    // so add_source can present the token proving the path is dialog-derived.
    const picked = await pickFolderDialog();
    setup.localPath = picked.path;
    setup.localPathToken = picked.token;
    if (!setup.sourceDisplayName) {
      setup.sourceDisplayName = baseName(picked.path);
    }
    // A destination with no browsable tree (a local folder) still needs a
    // non-empty destination folder id on the source row, so derive it from the
    // source's own name - the same choice a Drive user would have made by hand,
    // and shown read-only below so it is never a surprise.
    if (!setup.backendSupportsFolderPicker) {
      setup.driveFolderId = setup.destinationFolderIdFor(picked.path);
      setup.driveFolderPath = `${setup.localFolderRoot ?? ""}/${setup.driveFolderId}`;
    }
  } catch {
    // A cancel (or dialog error) surfaces as a rejected command; leave the path
    // unset so the step's "Next" stays disabled. No hard error shown for a
    // cancel - the user simply did not pick a folder.
  } finally {
    pickingFolder.value = false;
  }
}

// The Drive destination is chosen via the shared DriveFolderPicker (breadcrumb
// browser), which writes setup.driveFolderId / setup.driveFolderPath through
// v-model. A listing failure is surfaced here as a stable SPEC s24 code (mapped
// to errors.${code}.long), falling back to drive.unreachable.
function onDrivePickerError(e: unknown): void {
  setup.errorCode =
    e && typeof e === "object" && "code" in e
      ? String((e as { code: unknown }).code)
      : "drive.unreachable";
  setup.errorDetail = toErrorMessage(e);
}

// --- Navigation --------------------------------------------------------------

function onCredentialsComplete(): void {
  // Sign-in resolved; move to the source step.
  if (setup.step === "credentials") setup.next();
}

/**
 * The S3 branch's "sign in": one call that validates the settings, proves the
 * key pair reaches the bucket, stores the secret in the OS keychain and writes
 * the account. Only on success does the wizard advance - a rejected credential
 * leaves the user on the form with the specific reason, exactly as a failed
 * OAuth consent does.
 */
async function onS3Submit(req: CreateS3AccountRequest): Promise<void> {
  const ok = await setup.createS3Account(req);
  if (ok && setup.step === "credentials") setup.next();
}

/**
 * The local-folder branch's "sign in": one call that validates the folder,
 * proves it is writable, stamps its destination marker and writes the account.
 * Only on success does the wizard advance - a folder that cannot be written to
 * leaves the user on the form with the specific reason, exactly as a failed
 * OAuth consent does.
 */
async function onLocalFolderSubmit(req: CreateLocalFolderAccountRequest): Promise<void> {
  const ok = await setup.createLocalFolderAccount(req);
  if (ok && setup.step === "credentials") setup.next();
}

/**
 * The SSH (SFTP) branch's "sign in": one call that validates the settings,
 * PROVES a real SSH session can reach the server and the root path with a real
 * write, pins the host key, stores the secret in the OS keychain and writes
 * the account. Only on success does the wizard advance - a refused credential
 * or a missing root path leaves the user on the form with the specific
 * reason, exactly as a failed OAuth consent does. The pinned fingerprint (and,
 * if the root already held a Driven backup, the adoption note) render on the
 * SOURCE step below, since this call also advances past step 2's own card.
 */
async function onSftpSubmit(req: CreateSftpAccountRequest): Promise<void> {
  const ok = await setup.createSftpAccount(req);
  if (ok && setup.step === "credentials") setup.next();
}

async function onNext(): Promise<void> {
  if (!canAdvance.value) return;
  if (setup.step === "welcome") {
    // Open the server-side wizard session NOW, stamped with the destination the
    // user just chose. A failure is not a hard stop - the credentials step's
    // `connectAccount` re-begins and surfaces the error there.
    try {
      await setup.begin();
    } catch {
      // Deliberately swallowed; see above.
    }
  }
  if (setup.step === "encryption") {
    // Create the first source with its encryption flag, then advance to confirm.
    try {
      await setup.createFirstSource();
    } catch {
      return; // error surfaced via errorLong; stay on the step.
    }
  }
  setup.next();
}

async function onFinish(): Promise<void> {
  // B3: never finish while a displayed recovery phrase is un-acknowledged.
  if (!setup.canFinish) return;
  // M9c D4 (DATA-SAFETY): if the first encrypted source is pending a backend
  // recovery-phrase ack, acknowledge it FIRST - this ENABLES the until-now-disabled
  // source (the backend rejects the ack unless a real reveal was recorded). Only
  // then can the initial sync back it up. If the ack fails, stay on confirm.
  if (setup.pendingRecoveryAck) {
    try {
      await setup.ackRecoveryPhrase();
    } catch {
      return; // error surfaced via errorLong; stay on confirm.
    }
  }
  try {
    await setup.startInitialSync();
  } catch {
    return; // stay on confirm; error is shown.
  }
  await router.push("/activity");
}

function onPhraseAck(value: boolean): void {
  setup.acknowledgePhrase(value);
}

// R3-P1-1: the reveal component signals when the phrase has been revealed (or
// re-locked because it changed). Finish gates on reveal AND acknowledge.
function onPhraseRevealed(value: boolean): void {
  setup.markPhraseRevealed(value);
}

// M9c D4: the BACKEND reveal action threaded into RecoveryPhraseReveal (only for a
// pending-ack source). It records the reveal the ack gate requires.
async function revealPhraseAction(): Promise<void> {
  await setup.revealRecoveryPhrase();
}

// M9c D4: surface a backend reveal error.
function onPhraseRevealError(code: unknown): void {
  setup.errorCode =
    code && typeof code === "object" && "code" in code
      ? String((code as { code: unknown }).code)
      : "internal.bug";
  setup.errorDetail = toErrorMessage(code);
}

function baseName(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : p;
}
</script>

<template>
  <section class="mx-auto max-w-2xl space-y-6">
    <header class="space-y-1">
      <h1 class="text-2xl font-semibold text-zinc-900 dark:text-zinc-100">
        {{ t("wizard.title") }}
      </h1>
      <p class="text-sm text-teal-700 dark:text-teal-300">
        {{ t("wizard.stepLabel", { current, total }) }}
      </p>
      <!-- Step progress: a teal bar that fills as the wizard advances. -->
      <div class="mt-1 h-1.5 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-800">
        <div
          class="h-full rounded-full bg-teal-600 transition-all"
          :style="{ width: `${(current / total) * 100}%` }"
        />
      </div>
    </header>

    <!-- Step 1: Welcome + backup-destination choice -->
    <div v-if="setup.step === 'welcome'" :class="CARD" class="space-y-4">
      <h2 class="text-lg font-medium text-zinc-900 dark:text-zinc-100">
        {{ t("wizard.step1.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step1.body") }}
      </p>
      <BackendPicker
        v-model:selected="setup.backendId"
        :backends="setup.backends"
        :loading="setup.busy"
        @load="setup.loadBackends()"
      />
    </div>

    <!-- Step 2: credentials. Which form depends on the destination chosen on
         step 1: Google Drive runs the BYO-OAuth consent flow; a local or
         removable folder just needs the folder (no credential at all); an
         S3-compatible destination takes an access key pair; an SSH (SFTP)
         destination takes a password or a private key. Every non-OAuth
         destination is an EXPLICIT branch keyed on its id, not a two-valued
         OAuth/not-OAuth split whose `v-else` used to catch "anything else" -
         with three non-OAuth destinations now offered, that catch-all would
         silently route SFTP into the S3 form and submit the wrong request
         shape. The terminal `v-else` below is for a destination this build's
         picker offers but this wizard has no form for - it should not happen
         (they ship together), but it must fail visibly, not as a blank card
         above a dead Next. -->
    <div v-else-if="setup.step === 'credentials'" :class="CARD" class="space-y-3">
      <h2 class="text-lg font-medium text-zinc-900 dark:text-zinc-100">
        {{ credentialsStepTitle }}
      </h2>
      <CredentialsWalkthrough v-if="setup.backendUsesOauth" @complete="onCredentialsComplete" />
      <LocalFolderForm
        v-else-if="setup.backendId === LOCAL_FOLDER_BACKEND_ID"
        :busy="setup.busy"
        :error-message="errorLong"
        @submit="onLocalFolderSubmit"
      />
      <template v-else-if="setup.backendId === S3_BACKEND_ID">
        <S3CredentialsForm :busy="setup.busy" :error-message="errorLong" @submit="onS3Submit" />
        <p class="text-xs text-amber-700 dark:text-amber-400">
          {{ t("s3Setup.trashWarning") }}
        </p>
      </template>
      <SshCredentialsForm
        v-else-if="setup.backendId === SFTP_BACKEND_ID"
        :busy="setup.busy"
        :error-message="errorLong"
        @submit="onSftpSubmit"
      />
      <p v-else class="text-sm text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step2.unsupportedBody") }}
      </p>
    </div>

    <!-- Step 3: First backup source -->
    <div v-else-if="setup.step === 'source'" :class="CARD" class="space-y-4">
      <h2 class="text-lg font-medium text-zinc-900 dark:text-zinc-100">
        {{ t("wizard.step3.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step3.body") }}
      </p>

      <!-- SSH (SFTP)'s TOFU (trust-on-first-use) confirmation: step 2's own
           card is unreachable by the time there is anything to show (this
           step is only reached AFTER `onSftpSubmit` advances past it), so the
           pinned host key - and, if the root already held a Driven backup,
           the adoption note - render here instead.

           Gated on backendId TOO, not just the fingerprint being set: Back
           from this step, then creating an S3 or local-folder account
           instead, leaves `sftpHostKeyFingerprint` set from the abandoned
           SFTP attempt (it is cleared only by `reset()` or a later SUCCESSFUL
           SFTP create) - without this second gate that stale value renders
           here for a destination it has nothing to do with. -->
      <p
        v-if="setup.backendId === SFTP_BACKEND_ID && setup.sftpHostKeyFingerprint"
        class="rounded-md bg-teal-50 px-3 py-2 text-sm text-teal-800 dark:bg-teal-950/40 dark:text-teal-300"
        data-testid="sftp-fingerprint-confirmation"
      >
        {{ t("sftpSetup.fingerprintConfirmed", { fingerprint: setup.sftpHostKeyFingerprint }) }}
        <span v-if="setup.sftpAdopted" data-testid="sftp-adopted-note">
          {{ t("sftpSetup.adoptedNote") }}
        </span>
      </p>

      <div class="space-y-2">
        <button
          type="button"
          :class="SECONDARY_BTN"
          :disabled="pickingFolder"
          data-testid="wizard-choose-folder"
          @click="chooseLocalFolder"
        >
          {{ t("wizard.step3.chooseFolderButton") }}
        </button>
        <p v-if="setup.localPath" class="break-all text-sm text-zinc-600 dark:text-zinc-400">
          {{ setup.localPath }}
        </p>
      </div>

      <div class="space-y-2">
        <span class="block text-sm font-medium text-zinc-700 dark:text-zinc-200">{{
          t("wizard.step3.driveDestinationLabel")
        }}</span>
        <!-- Only destinations that can enumerate a folder tree get a picker.
             Drive browses folders and S3 browses key prefixes, so both do; a
             local or removable folder does not - its root was already chosen when
             the account was created, and there is nothing below it to choose. -->
        <DriveFolderPicker
          v-if="setup.backendSupportsFolderPicker"
          v-model:folder-id="setup.driveFolderId"
          v-model:folder-path="setup.driveFolderPath"
          v-model:drive-id="setup.driveId"
          :account-id="setup.accountId"
          :backend-kind="setup.backendId"
          @error="onDrivePickerError"
        />
        <!-- A destination with no browsable tree (a local or removable folder):
             the root was already chosen on step 2, so the only thing left to
             show is WHERE inside it this source will land. Read-only, but
             visible, so the resolved path is never a surprise. -->
        <p
          v-else
          class="break-all text-sm text-zinc-600 dark:text-zinc-400"
          data-testid="local-destination-path"
        >
          {{
            setup.driveFolderPath || setup.localFolderRoot || t("wizard.step3.chooseFolderFirst")
          }}
        </p>
      </div>
    </div>

    <!-- Step 4: Encryption opt-in + recovery phrase -->
    <div v-else-if="setup.step === 'encryption'" :class="CARD" class="space-y-3">
      <h2 class="text-lg font-medium text-zinc-900 dark:text-zinc-100">
        {{ t("wizard.step4.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step4.body") }}
      </p>

      <label class="flex items-center gap-2 text-sm text-zinc-700 dark:text-zinc-200">
        <input v-model="setup.encryptionEnabled" type="checkbox" class="h-4 w-4 accent-teal-600" />
        <span>{{ t("wizard.step4.enableLabel") }}</span>
      </label>

      <p
        v-if="setup.encryptionEnabled"
        class="rounded-md bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:bg-amber-950/40 dark:text-amber-300"
      >
        {{ t("wizard.step4.recoveryWarning") }}
      </p>
      <!-- B3: the phrase is NOT shown here - it does not exist until the source
           is created (on Next). It is revealed on the confirm step below. -->
    </div>

    <!-- Step 5: Confirm + recovery-phrase reveal + start initial sync -->
    <div v-else :class="CARD" class="space-y-3">
      <h2 class="text-lg font-medium text-zinc-900 dark:text-zinc-100">
        {{ t("wizard.step5.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step5.body") }}
      </p>

      <!-- B3: the source's encryption opt-in generated a recovery phrase - show
           it exactly once and gate Finish on the user acknowledging they saved
           it. The reveal renders only when a real phrase was returned. -->
      <RecoveryPhraseReveal
        v-if="setup.hasRecoveryPhrase"
        :phrase="setup.recoveryPhrase ?? undefined"
        :confirmed="setup.phraseAcknowledged"
        :reveal-action="setup.pendingRecoveryAck ? revealPhraseAction : undefined"
        @update:confirmed="onPhraseAck"
        @update:revealed="onPhraseRevealed"
        @reveal-error="onPhraseRevealError"
      />
    </div>

    <p v-if="errorLong" class="text-sm text-red-600" role="alert">
      {{ errorLong }}
    </p>
    <!-- Technical detail under the localized line: the stable code plus the
         backend's redacted message, so distinct failures behind one code are
         tellable apart and a screenshot alone is diagnosable. -->
    <p
      v-if="errorLong && errorDetailLine"
      class="break-words font-mono text-xs text-zinc-500 dark:text-zinc-400"
      data-testid="setup-error-detail"
    >
      {{ errorDetailLine }}
    </p>

    <footer class="flex justify-between">
      <button
        type="button"
        :class="SECONDARY_BTN"
        :disabled="!setup.canGoBack || setup.busy"
        @click="setup.back()"
      >
        {{ t("common.back") }}
      </button>

      <button
        v-if="setup.step === 'confirm'"
        type="button"
        :class="PRIMARY_BTN"
        :disabled="setup.busy || !setup.canFinish"
        @click="onFinish"
      >
        {{ t("wizard.step5.startButton") }}
      </button>
      <button
        v-else
        type="button"
        :class="PRIMARY_BTN"
        :disabled="!canAdvance || setup.busy"
        @click="onNext"
      >
        {{ t("common.next") }}
      </button>
    </footer>
  </section>
</template>
