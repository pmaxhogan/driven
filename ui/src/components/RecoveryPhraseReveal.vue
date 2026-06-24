<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

// Recovery-phrase reveal (DESIGN s7.3, s8.5 step 4). Shows the BIP39 phrase the
// backend returns on encryption opt-in exactly once, hidden behind a
// click-to-reveal so a casual screenshot never leaks it. The phrase is supplied
// by the parent (AddSourceWizard / SetupWizard) which obtained it from the
// backend's master_key_to_phrase; this component never derives or stores it.
// `confirmed` is a v-model so the parent can gate "next" on the user attesting
// they saved the words.
const { t } = useI18n();

const props = withDefaults(
  defineProps<{ phrase?: string[]; confirmed?: boolean }>(),
  { phrase: () => [], confirmed: false },
);

const emit = defineEmits<{ "update:confirmed": [value: boolean] }>();

const revealed = ref(false);
const copied = ref(false);

const hasPhrase = computed(() => props.phrase.length > 0);

function toggle(): void {
  revealed.value = !revealed.value;
}

async function copy(): Promise<void> {
  if (!hasPhrase.value) return;
  const text = props.phrase.join(" ");
  try {
    if (
      typeof navigator !== "undefined" &&
      navigator.clipboard &&
      typeof navigator.clipboard.writeText === "function"
    ) {
      await navigator.clipboard.writeText(text);
      copied.value = true;
    }
  } catch {
    copied.value = false;
  }
}

function onConfirmToggle(event: Event): void {
  const target = event.target as HTMLInputElement;
  emit("update:confirmed", target.checked);
}
</script>

<template>
  <div class="space-y-3 rounded border p-4">
    <h3 class="text-sm font-medium">
      {{ t("recoveryPhrase.title") }}
    </h3>
    <p class="text-xs text-zinc-500">
      {{ t("recoveryPhrase.instructions") }}
    </p>

    <div class="flex gap-2">
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="!hasPhrase"
        @click="toggle"
      >
        {{
          revealed
            ? t("recoveryPhrase.hideButton")
            : t("recoveryPhrase.revealButton")
        }}
      </button>
      <button
        v-if="revealed"
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="!hasPhrase"
        @click="copy"
      >
        {{ t("recoveryPhrase.copyButton") }}
      </button>
    </div>

    <ol
      v-if="revealed && hasPhrase"
      class="grid grid-cols-3 gap-1 text-sm"
      data-testid="phrase-words"
    >
      <li
        v-for="(word, index) in props.phrase"
        :key="index"
        class="font-mono"
      >
        {{ index + 1 }}. {{ word }}
      </li>
    </ol>

    <label class="flex items-center gap-2 text-sm">
      <input
        type="checkbox"
        :checked="props.confirmed"
        @change="onConfirmToggle"
      >
      {{ t("recoveryPhrase.confirmedLabel") }}
    </label>
  </div>
</template>
