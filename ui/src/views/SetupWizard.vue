<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import RecoveryPhraseReveal from "../components/RecoveryPhraseReveal.vue";
import { useSetupStore, WIZARD_STEPS } from "../stores/setup";

// Setup wizard (SPEC s25 /setup; DESIGN s8.5 5-step wizard). M6 shell: the step
// scaffolding + navigation are wired to the setup store; each step's rich form
// is filled in by the accounts implementer. The encryption step embeds the
// recovery-phrase reveal component.
const { t } = useI18n();
const setup = useSetupStore();

const total = WIZARD_STEPS.length;
const current = computed(() => setup.stepIndex + 1);
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

    <div
      v-else-if="setup.step === 'credentials'"
      class="space-y-2"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step2.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step2.body") }}
      </p>
    </div>

    <div
      v-else-if="setup.step === 'source'"
      class="space-y-2"
    >
      <h2 class="text-lg font-medium">
        {{ t("wizard.step3.title") }}
      </h2>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("wizard.step3.body") }}
      </p>
    </div>

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
      <RecoveryPhraseReveal />
    </div>

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

    <footer class="flex justify-between">
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="!setup.canGoBack"
        @click="setup.back()"
      >
        {{ t("common.back") }}
      </button>
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="!setup.canGoNext"
        @click="setup.next()"
      >
        {{ t("common.next") }}
      </button>
    </footer>
  </section>
</template>
