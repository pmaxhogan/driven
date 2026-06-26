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

// Design-system class strings (shared verbatim across slices for consistency).
const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";

const props = withDefaults(
  defineProps<{
    phrase?: string[];
    confirmed?: boolean;
    // M9c D4: an optional async reveal action the parent supplies (the backend
    // `revealRecoveryPhrase`). When present, the FIRST reveal awaits it and only
    // latches `everRevealed` on success - so the recorded backend reveal that the
    // ack gate requires actually happens. When absent, reveal is purely
    // client-side (the existing behaviour; used where no backend reveal applies).
    //
    // R9-P1-2: the action RETURNS the revealed phrase (the 24 words). The
    // post-restart SourceTable path supplies the phrase via this return value, and
    // `toggle` latches `revealed`/`everRevealed` deterministically FROM the returned
    // value - it does NOT wait for Vue to deliver the parent's `phrase` prop, which
    // may land on a later tick and previously left the ack control locked. The
    // action may also return void (it set the parent prop itself), in which case
    // the latch falls back to the prop once it arrives.
    revealAction?: () => Promise<string[] | void>;
  }>(),
  { phrase: () => [], confirmed: false, revealAction: undefined }
);

const emit = defineEmits<{
  "update:confirmed": [value: boolean];
  "update:revealed": [value: boolean];
  // M9c D4: surfaced when the backend reveal action rejects, so the parent can
  // show the localized error.
  "reveal-error": [code: unknown];
}>();

const revealed = ref(false);
// M9c D4: true while the async backend reveal is in flight (disables the button).
const revealing = ref(false);
// R3-P1-1: latches true the first time the user reveals the phrase; never
// auto-clears on hide (re-hiding does not "un-see" the words). Only a phrase
// change resets it.
const everRevealed = ref(false);
const copied = ref(false);

// R9-P1-2: the phrase value we latched the reveal against. Set when `toggle`
// latches (from the action's returned phrase or the prop). The prop watcher uses
// it to tell an already-latched reveal's own prop delivery (same words, must NOT
// re-lock) apart from a genuinely fresh/cleared phrase (must re-lock). `null`
// means "nothing latched yet".
const latchedPhrase = ref<string[] | null>(null);

const hasPhrase = computed(() => props.phrase.length > 0);

// R9-P1-2: compare two phrase word-lists for equality (order-sensitive).
function samePhrase(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

// R9-P1-2: latch the reveal against a concrete phrase. Idempotent: emits
// `update:revealed` only on the first latch. The phrase here is the source of
// truth for the latch, so a later prop delivery of the SAME words cannot re-lock.
function latchReveal(phrase: string[]): void {
  if (phrase.length === 0) return;
  latchedPhrase.value = phrase.slice();
  if (!everRevealed.value) {
    everRevealed.value = true;
    emit("update:revealed", true);
  }
}

// M9c D4 / R7-P2-1: a backend reveal action is supplied. When present, the reveal
// click itself FETCHES + records the phrase (the post-restart SourceTable case),
// so the Reveal button must be clickable even before any phrase is loaded - the
// action populates `phrase`. Without an action, reveal is purely client-side and
// still requires a phrase to be present.
const hasRevealAction = computed(() => typeof props.revealAction === "function");

// The Reveal button is usable when there is a phrase to show OR a backend action
// that will supply one. (Re-hiding stays available once revealed.)
const canReveal = computed(() => revealed.value || hasPhrase.value || hasRevealAction.value);

// R3-P1-1: the acknowledge checkbox is usable only once the phrase has been
// revealed AND a real phrase is present.
// R9-P1-2: "a real phrase is present" is satisfied by the prop OR by the phrase we
// latched from the reveal action's return value - the post-restart path latches
// before Vue delivers the prop, and the ack control must not stay locked across
// that tick.
const ackEnabled = computed(
  () => everRevealed.value && (hasPhrase.value || (latchedPhrase.value?.length ?? 0) > 0)
);

async function toggle(): Promise<void> {
  // Hiding is always allowed and never un-sees the words.
  if (revealed.value) {
    revealed.value = false;
    return;
  }
  // Revealing for the FIRST time with a backend reveal action: await it so the
  // backend records the reveal AND (post-restart) RETURNS the phrase (the ack gate
  // depends on the recorded reveal). Only latch on success; a rejected backend
  // reveal leaves the phrase hidden + un-latched. The phrase need NOT be present
  // in the prop yet - the action's return value supplies it.
  let returnedPhrase: string[] | void = undefined;
  if (!everRevealed.value && hasRevealAction.value) {
    revealing.value = true;
    try {
      // props.revealAction is guaranteed a function by hasRevealAction.
      returnedPhrase = await props.revealAction!();
    } catch (e) {
      revealing.value = false;
      emit("reveal-error", e);
      return;
    }
    revealing.value = false;
  }
  revealed.value = true;
  // R9-P1-2: latch DETERMINISTICALLY from the phrase the reveal yielded - the
  // action's return value if it gave one, otherwise the prop (the client-side case
  // where the parent already had the phrase). This does NOT depend on Vue having
  // delivered an updated `phrase` prop yet, so the post-restart path no longer
  // leaves the ack control locked across the prop tick. A reveal that yielded no
  // phrase at all does not unlock ack.
  if (!everRevealed.value) {
    const effectivePhrase =
      Array.isArray(returnedPhrase) && returnedPhrase.length > 0 ? returnedPhrase : props.phrase;
    if (effectivePhrase.length > 0) {
      latchReveal(effectivePhrase.slice());
    }
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
//
// R9-P1-2: EXCEPT when the incoming prop is just the delivery of the phrase we
// already latched (the post-restart path latches from the reveal action's return
// value, then the parent sets the prop to those SAME words on a later tick). That
// delivery must NOT re-lock the already-revealed/acknowledged state. Only a
// genuinely different (or cleared) phrase re-locks.
watch(
  () => props.phrase,
  (next) => {
    if (
      everRevealed.value &&
      latchedPhrase.value !== null &&
      samePhrase(next, latchedPhrase.value)
    ) {
      // Same words we already latched: keep the latch + ack intact.
      return;
    }
    revealed.value = false;
    everRevealed.value = false;
    revealing.value = false;
    copied.value = false;
    latchedPhrase.value = null;
    emit("update:revealed", false);
    if (props.confirmed) emit("update:confirmed", false);
  }
);
</script>

<template>
  <div
    class="space-y-3 rounded-lg border border-zinc-200 bg-white p-4 shadow-sm dark:border-zinc-800 dark:bg-zinc-900"
  >
    <h3 class="text-sm font-medium text-zinc-900 dark:text-zinc-100">
      {{ t("recoveryPhrase.title") }}
    </h3>
    <p class="text-xs text-zinc-500 dark:text-zinc-400">
      {{ t("recoveryPhrase.instructions") }}
    </p>

    <div class="flex flex-wrap gap-2">
      <button
        type="button"
        :class="SECONDARY_BTN"
        :disabled="!canReveal || revealing"
        @click="toggle"
      >
        {{
          revealing
            ? t("recoveryPhrase.revealingButton")
            : revealed
              ? t("recoveryPhrase.hideButton")
              : t("recoveryPhrase.revealButton")
        }}
      </button>
      <button
        v-if="revealed"
        type="button"
        :class="SECONDARY_BTN"
        :disabled="!hasPhrase"
        @click="copy"
      >
        {{ t("recoveryPhrase.copyButton") }}
      </button>
    </div>

    <ol
      v-if="revealed && hasPhrase"
      class="grid grid-cols-2 gap-1 rounded-md border border-zinc-200 bg-zinc-50 p-3 text-sm sm:grid-cols-3 dark:border-zinc-800 dark:bg-zinc-950"
      data-testid="phrase-words"
    >
      <li
        v-for="(word, index) in props.phrase"
        :key="index"
        class="font-mono text-zinc-900 dark:text-zinc-100"
      >
        <span class="text-zinc-400 dark:text-zinc-500">{{ index + 1 }}.</span> {{ word }}
      </li>
    </ol>

    <label
      class="flex items-center gap-2 text-sm text-zinc-700 dark:text-zinc-200"
      :class="{ 'opacity-50': !ackEnabled }"
    >
      <input
        type="checkbox"
        class="h-4 w-4 accent-teal-600"
        :checked="props.confirmed"
        :disabled="!ackEnabled"
        data-testid="phrase-ack"
        @change="onConfirmToggle"
      />
      {{ t("recoveryPhrase.confirmedLabel") }}
    </label>
    <p v-if="!ackEnabled" class="text-xs text-zinc-500 dark:text-zinc-400">
      {{ t("recoveryPhrase.revealFirstHint") }}
    </p>
  </div>
</template>
