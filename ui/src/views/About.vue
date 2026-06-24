<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";
import type { UpdateInfo } from "../ipc/types";

// About view (SPEC s11.6, s25 /about; DESIGN s8.2 About tab). M6 shell: version,
// update channel, check-for-updates, license, release notes, display language.
// The settings implementer wires the real version/license values + the release
// notes viewer; the shell already calls the IPC wrappers so the wiring is live.
const { t } = useI18n();

const checking = ref(false);
const update = ref<UpdateInfo | null>(null);
const checked = ref(false);

async function checkForUpdates(): Promise<void> {
  checking.value = true;
  try {
    update.value = await ipc.checkForUpdates();
    checked.value = true;
  } finally {
    checking.value = false;
  }
}
</script>

<template>
  <section class="space-y-4">
    <h1 class="text-2xl font-semibold">
      {{ t("about.title") }}
    </h1>

    <div class="space-y-2">
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="checking"
        @click="checkForUpdates"
      >
        {{ t("about.checkForUpdatesButton") }}
      </button>
      <p
        v-if="checking"
        class="text-sm text-zinc-500"
      >
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="checked && update"
        class="text-sm"
      >
        {{ t("about.updateAvailable", { version: update.version }) }}
      </p>
      <p
        v-else-if="checked"
        class="text-sm text-zinc-500"
      >
        {{ t("about.upToDate") }}
      </p>
    </div>

    <p class="text-sm text-zinc-500">
      {{ t("about.moreLanguagesComing") }}
    </p>
  </section>
</template>
