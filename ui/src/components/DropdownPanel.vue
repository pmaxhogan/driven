<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";

// The app's first dropdown primitive (issue #304). Everything overlay-shaped
// here so far has been a modal (ChangelogModal.vue: a full-screen scrim plus a
// centred panel); a top-bar menu needs the opposite - anchored, light, and
// dismissible without a scrim swallowing the rest of the UI.
//
// Accessibility is the whole point of factoring it out, so the next dropdown
// inherits it instead of re-deriving it:
//   - the trigger is a real <button> with `aria-expanded` + `aria-controls`,
//   - Escape closes and returns focus to the trigger (so keyboard users are not
//     stranded inside a panel they cannot see),
//   - a pointer press outside closes,
//   - focus rings match the shell's teal outline convention.
//
// Deliberately NOT `role="menu"`: the panel holds prose, progress bars, and
// several controls per row, and menu semantics would promise arrow-key item
// navigation that does not exist. It is a labelled group the user tabs through.

const props = withDefaults(
  defineProps<{
    /** Accessible name of the panel (the group's aria-label). */
    panelLabel: string;
    /** Accessible name of the trigger button. */
    triggerLabel: string;
    /** Classes for the trigger button (the caller owns its look). */
    triggerClass?: string;
    /** Width utility for the panel. Defaults to the ~360px the queue uses. */
    panelClass?: string;
    /** Stable id fragment so `aria-controls` is unique on a page with two. */
    id?: string;
  }>(),
  {
    triggerClass: "",
    panelClass: "w-[360px] max-w-[calc(100vw-2rem)]",
    id: "dropdown",
  }
);

/** Two-way `open`, so a parent can close the panel after an action (or open it
 * programmatically) while the component still manages the default toggling. */
const open = defineModel<boolean>("open", { default: false });

const root = ref<HTMLElement | null>(null);
const trigger = ref<HTMLButtonElement | null>(null);

const panelId = computed(() => `${props.id}-panel`);

function close(): void {
  open.value = false;
}

/** Close AND hand focus back to the trigger - the keyboard path, where leaving
 * focus on a removed element would drop the user at the top of the document. */
function closeAndRefocus(): void {
  const wasOpen = open.value;
  close();
  if (wasOpen) trigger.value?.focus();
}

function toggle(): void {
  open.value = !open.value;
}

function onPointerDown(event: MouseEvent): void {
  const target = event.target as Node | null;
  if (target && root.value && !root.value.contains(target)) close();
}

function onKeydown(event: KeyboardEvent): void {
  if (event.key === "Escape") closeAndRefocus();
}

// Listeners exist only while the panel is open: a closed dropdown must cost the
// document nothing, and a stale listener on an unmounted panel is a leak.
watch(
  open,
  (isOpen) => {
    if (isOpen) {
      document.addEventListener("mousedown", onPointerDown);
      document.addEventListener("keydown", onKeydown);
    } else {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKeydown);
    }
  },
  { immediate: true }
);

onBeforeUnmount(() => {
  document.removeEventListener("mousedown", onPointerDown);
  document.removeEventListener("keydown", onKeydown);
});

defineExpose({ close });
</script>

<template>
  <div ref="root" class="relative">
    <button
      ref="trigger"
      type="button"
      :class="triggerClass"
      :aria-expanded="open"
      :aria-controls="panelId"
      :aria-label="triggerLabel"
      data-testid="dropdown-trigger"
      @click="toggle"
    >
      <slot name="trigger" :open="open" />
    </button>
    <!-- Anchored to the trigger's right edge so a panel wider than its button
         never overflows the window. z-40 sits above the sticky header (z-30)
         and below the modal overlays (z-50). -->
    <div
      v-if="open"
      :id="panelId"
      role="group"
      :aria-label="panelLabel"
      :class="panelClass"
      class="absolute right-0 z-40 mt-2 max-h-[70vh] overflow-y-auto rounded-md border border-zinc-200 bg-white shadow-lg dark:border-zinc-700 dark:bg-zinc-900"
      data-testid="dropdown-panel"
    >
      <slot :close="close" />
    </div>
  </div>
</template>
