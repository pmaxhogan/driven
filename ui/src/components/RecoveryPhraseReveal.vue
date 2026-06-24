<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

// Recovery-phrase reveal (DESIGN s7.3, s8.5 step 4). Shows the BIP39 phrase the
// backend returns on encryption opt-in exactly once, hidden behind a
// click-to-reveal so a casual screenshot never leaks it. The phrase is supplied
// by the parent (AddSourceWizard / SetupWizard) which obtained it from the
// backend's master_key_to_phrase; this component never derives or stores it.
// `confirmed` is a v-model so the parent can gate "next" on the user attesting
// they saved the words.
//
// R3-P1-1: the acknowledge checkbox is DISABLED until the phrase has actually
// been REVEALED at least once. A user must not be able to attest "I saved my
// recovery phrase" while it is still hidden - that would let them start
// encrypted backups they can never restore. The component tracks `everRevealed`
// and emits `update:revealed` so the parent (store / wizard) can gate Finish on
// reveal AND acknowledge. When the phrase changes/clears, both the reveal state
// and the acknowledgement are reset (re-locked).
const { t } = useI18n();

const props = withDefaults(
  defineProps<{ phrase?: string[]; confirmed?: boolean }>(),
  { phrase: () => [], confirmed: false },
);

const emit = defineEmits<{
  "update:confirmed": [value: boolean];
  "update:revealed": [value: boolean];
}>();

const revealed = ref(false);
// R3-P1-1: latches true the first time the user reveals the phrase; never
// auto-clears on hide (re-hiding does not "un-see" the words). Only a phrase
// change resets it.
const everRevealed = ref(false);
const copied = ref(false);

const hasPhrase = computed(() => props.phrase.length > 0);

// R3-P1-1: the acknowledge checkbox is usable only once the phrase has been
// revealed AND a real phrase is present.
const ackEnabled = computed(() => everRevealed.value && hasPhrase.value);

function toggle(): void {
  revealed.value = !revealed.value;
  if (revealed.value && hasPhrase.value && !everRevealed.value) {
    everRevealed.value = true;
    emit("update:revealed", true);
  }
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
  // R3-P1-1: ignore a toggle that arrives while the checkbox is disabled (the
  // DOM `disabled` attribute already blocks user input; this is a belt-and-
  // braces guard so a programmatic event can never tick the box pre-reveal).
  if (!ackEnabled.value) {
    if (props.confirmed) emit("update:confirmed", false);
    return;
  }
  emit("update:confirmed", target.checked);
}

// R3-P1-1: when the phrase changes or clears, re-lock everything - a fresh
// phrase must be revealed and acknowledged anew. Reset local reveal state and
// signal the parent to clear both `revealed` and `confirmed`.
watch(
  () => props.phrase,
  () => {
    revealed.value = false;
    everRevealed.value = false;
    copied.value = false;
    emit("update:revealed", false);
    if (props.confirmed) emit("update:confirmed", false);
  },
);
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

    <label
      class="flex items-center gap-2 text-sm"
      :class="{ 'opacity-50': !ackEnabled }"
    >
      <input
        type="checkbox"
        :checked="props.confirmed"
        :disabled="!ackEnabled"
        data-testid="phrase-ack"
        @change="onConfirmToggle"
      >
      {{ t("recoveryPhrase.confirmedLabel") }}
    </label>
    <p
      v-if="!ackEnabled"
      class="text-xs text-zinc-500"
    >
      {{ t("recoveryPhrase.revealFirstHint") }}
    </p>
  </div>
</template>
