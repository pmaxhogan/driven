<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

import CredentialsWalkthrough from "../components/CredentialsWalkthrough.vue";
import RecoveryPhraseReveal from "../components/RecoveryPhraseReveal.vue";
import { pickDriveFolder } from "../ipc/commands";
import { useSetupStore, WIZARD_STEPS } from "../stores/setup";

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
// IPC path safety (SPEC s11.6.1): the local folder is chosen via the
// tauri-plugin-dialog directory picker, so add_source receives a dialog-derived
// path the backend validates - never a webview-supplied string.

const { t } = useI18n();
const router = useRouter();
const setup = useSetupStore();

const total = WIZARD_STEPS.length;
const current = computed(() => setup.stepIndex + 1);

const pickingFolder = ref(false);
const loadingDrive = ref(false);

// Begin the wizard session up front so the credentials step has a session id
// (SPEC s11.1 begin_add_account_wizard). Idempotent: only begins once.
onMounted(async () => {
  setup.reset();
  try {
    await setup.begin();
  } catch {
    // begin failure surfaces via the credentials step the first time the user
    // tries to sign in (connectAccount re-begins if no session); no hard stop
    // here so the welcome step still renders.
  }
});

const errorLong = computed<string | null>(() =>
  setup.errorCode ? t(`errors.${setup.errorCode}.long`) : null,
);

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
      return !!setup.localPath && !!setup.driveFolderId;
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
    const selected = await openDialog({ directory: true, multiple: false });
    if (typeof selected === "string") {
      setup.localPath = selected;
      if (!setup.sourceDisplayName) {
        setup.sourceDisplayName = baseName(selected);
      }
    }
  } finally {
    pickingFolder.value = false;
  }
}

async function chooseDriveFolder(): Promise<void> {
  const acct = setup.accountId;
  if (!acct) return;
  loadingDrive.value = true;
  setup.clearError();
  try {
    const result = await pickDriveFolder(acct, null);
    setup.driveFolderId = result.currentFolderId;
    setup.driveFolderPath = result.currentFolderPath;
  } catch (e) {
    setup.errorCode =
      e && typeof e === "object" && "code" in e
        ? String((e as { code: unknown }).code)
        : "drive.unreachable";
  } finally {
    loadingDrive.value = false;
  }
}

// --- Navigation --------------------------------------------------------------

function onCredentialsComplete(): void {
  // Sign-in resolved; move to the source step.
  if (setup.step === "credentials") setup.next();
}

async function onNext(): Promise<void> {
  if (!canAdvance.value) return;
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
  try {
    await setup.startInitialSync();
  } catch {
    return; // stay on confirm; error is shown.
  }
  await router.push("/activity");
}

function baseName(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : p;
}
</script>

<template>
  <section class="mx-auto max-w-2xl space-y-6">
    <header class="space-y-1">
      <h1 class="text-2xl font-semibold">
        {{ t("wizard.title") }}
      </h1>
      <p class="text-sm text-zinc-500">
        {{ t("wizard.stepLabel", { current, total }) }}
      </p>
    </header>

    <!-- Step 1: Welcome -->
    <div
      v-if="setup.step === 'welcome'"
      class="space-y-2"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step1.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step1.body") }}
      </p>
    </div>

    <!-- Step 2: BYO credentials + sign-in -->
    <div
      v-else-if="setup.step === 'credentials'"
      class="space-y-3"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step2.title") }}
      </h2>
      <CredentialsWalkthrough @complete="onCredentialsComplete" />
    </div>

    <!-- Step 3: First backup source -->
    <div
      v-else-if="setup.step === 'source'"
      class="space-y-3"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step3.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step3.body") }}
      </p>

      <div class="space-y-2">
        <button
          type="button"
          class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
          :disabled="pickingFolder"
          @click="chooseLocalFolder"
        >
          {{ t("wizard.step3.chooseFolderButton") }}
        </button>
        <p
          v-if="setup.localPath"
          class="break-all text-sm text-zinc-600 dark:text-zinc-400"
        >
          {{ setup.localPath }}
        </p>
      </div>

      <div class="space-y-2">
        <span class="block text-sm font-medium">{{
          t("wizard.step3.driveDestinationLabel")
        }}</span>
        <button
          type="button"
          class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
          :disabled="loadingDrive || !setup.accountId"
          @click="chooseDriveFolder"
        >
          {{ t("settings.addSource.chooseDriveButton") }}
        </button>
        <p
          v-if="setup.driveFolderPath"
          class="break-all text-sm text-zinc-600 dark:text-zinc-400"
        >
          {{ setup.driveFolderPath }}
        </p>
      </div>
    </div>

    <!-- Step 4: Encryption opt-in + recovery phrase -->
    <div
      v-else-if="setup.step === 'encryption'"
      class="space-y-3"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step4.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step4.body") }}
      </p>

      <label class="flex items-center gap-2 text-sm">
        <input
          v-model="setup.encryptionEnabled"
          type="checkbox"
        >
        <span>{{ t("wizard.step4.enableLabel") }}</span>
      </label>

      <template v-if="setup.encryptionEnabled">
        <p class="text-sm text-amber-700 dark:text-amber-500">
          {{ t("wizard.step4.recoveryWarning") }}
        </p>
        <RecoveryPhraseReveal :phrase="setup.recoveryPhrase ?? undefined" />
      </template>
    </div>

    <!-- Step 5: Confirm + start initial sync -->
    <div
      v-else
      class="space-y-2"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step5.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step5.body") }}
      </p>
    </div>

    <p
      v-if="errorLong"
      class="text-sm text-red-600"
      role="alert"
    >
      {{ errorLong }}
    </p>

    <footer class="flex justify-between">
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
        :disabled="!setup.canGoBack || setup.busy"
        @click="setup.back()"
      >
        {{ t("common.back") }}
      </button>

      <button
        v-if="setup.step === 'confirm'"
        type="button"
        class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
        :disabled="setup.busy"
        @click="onFinish"
      >
        {{ t("wizard.step5.startButton") }}
      </button>
      <button
        v-else
        type="button"
        class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
        :disabled="!canAdvance || setup.busy"
        @click="onNext"
      >
        {{ t("common.next") }}
      </button>
    </footer>
  </section>
</template>
