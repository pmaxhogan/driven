<script setup lang="ts">
import { ref } from "vue";
import { useI18n } from "vue-i18n";

// Recovery-phrase reveal (DESIGN s7.3, s8.5 step 4). M6 shell: a click-to-reveal
// surface for the 24-word BIP39 phrase the backend returns on encryption opt-in.
// The accounts/crypto implementer feeds the actual phrase in (from
// master_key_to_phrase) and gates the "I have saved it" confirmation; the shell
// keeps the phrase hidden by default so a screenshot never leaks it.
const { t } = useI18n();

const props = defineProps<{ phrase?: string[] }>();
const revealed = ref(false);

function toggle(): void {
  revealed.value = !revealed.value;
}
</script>

<template>
  <div class="space-y-2 rounded border p-4">
    <h3 class="text-sm font-medium">
      {{ t("recoveryPhrase.title") }}
    </h3>
    <p class="text-xs text-zinc-500">
      {{ t("recoveryPhrase.instructions") }}
    </p>
    <button
      type="button"
      class="rounded border px-3 py-1.5 text-sm"
      @click="toggle"
    >
      {{ revealed ? t("recoveryPhrase.hideButton") : t("recoveryPhrase.revealButton") }}
    </button>
    <ol
      v-if="revealed && props.phrase"
      class="grid grid-cols-3 gap-1 text-sm"
    >
      <li
        v-for="(word, index) in props.phrase"
        :key="index"
      >
        {{ index + 1 }}. {{ word }}
      </li>
    </ol>
  </div>
</template>
