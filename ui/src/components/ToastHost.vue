<script setup lang="ts">
import { useI18n } from "vue-i18n";

import { useBackupToasts } from "../composables/useBackupToasts";
import { useToastsStore, type ToastKind } from "../stores/toasts";

// The toast stack: transient messages pinned to the bottom-right of the window,
// over whatever route is showing. Mounted ONCE in App.vue, so it is the natural
// home for the backup-event subscriptions that feed it (useBackupToasts) - the
// same app-lifetime guarantee App.vue's own subscriptions have, without adding
// another block to App.vue.
//
// A11y: the container is a permanently-mounted `aria-live="polite"` region, so a
// toast added to it is announced without stealing focus. It must exist BEFORE
// the first toast arrives for that to work - hence `v-if` on the individual
// toasts, never on the region itself. Each toast carries an X button labelled
// via `aria-label` (the icon is `aria-hidden`), and the auto-dismiss timer
// pauses on hover AND on focus, so a keyboard user tabbing to the X does not
// have the toast vanish mid-reach.
const { t } = useI18n();
const toasts = useToastsStore();

useBackupToasts();

// Per-kind color treatment, matching the shell's language: teal for success (the
// brand accent), amber for warning (the paused banner's palette), red for error,
// neutral zinc for info. Full class strings - never interpolated - so Tailwind's
// scanner keeps every variant.
const KIND_CLASS: Record<ToastKind, string> = {
  info: "border-zinc-300 bg-white text-zinc-800 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100",
  success:
    "border-teal-400 bg-teal-50 text-teal-900 dark:border-teal-500/60 dark:bg-teal-950/80 dark:text-teal-100",
  warning:
    "border-amber-400 bg-amber-50 text-amber-900 dark:border-amber-500/60 dark:bg-amber-950/80 dark:text-amber-100",
  error:
    "border-red-400 bg-red-50 text-red-900 dark:border-red-500/60 dark:bg-red-950/80 dark:text-red-100",
};

// Screen-reader-only severity prefix, so "Backup complete" and an error read
// differently to someone who cannot see the color.
const KIND_LABEL: Record<ToastKind, string> = {
  info: "toast.kind.info",
  success: "toast.kind.success",
  warning: "toast.kind.warning",
  error: "toast.kind.error",
};
</script>

<template>
  <div
    class="pointer-events-none fixed right-4 bottom-4 z-50 flex w-80 max-w-[calc(100vw-2rem)] flex-col gap-2"
    aria-live="polite"
    data-testid="toast-host"
  >
    <TransitionGroup name="driven-toast">
      <div
        v-for="toast in toasts.toasts"
        :key="toast.id"
        :class="KIND_CLASS[toast.kind]"
        class="pointer-events-auto flex items-start gap-2 rounded-md border px-3 py-2 text-sm shadow-lg"
        data-testid="toast"
        @mouseenter="toasts.pause(toast.id)"
        @mouseleave="toasts.resume(toast.id)"
        @focusin="toasts.pause(toast.id)"
        @focusout="toasts.resume(toast.id)"
      >
        <span class="sr-only">{{ t(KIND_LABEL[toast.kind]) }}</span>
        <p class="min-w-0 flex-1 break-words">{{ toast.message }}</p>
        <button
          type="button"
          class="-mr-1 shrink-0 rounded-sm p-0.5 opacity-70 transition-opacity hover:opacity-100 focus-visible:opacity-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-current"
          :aria-label="t('toast.dismiss')"
          data-testid="toast-dismiss"
          @click="toasts.dismiss(toast.id)"
        >
          <svg
            class="h-4 w-4"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.75"
            stroke-linecap="round"
            aria-hidden="true"
          >
            <path d="M4 4l8 8M12 4l-8 8" />
          </svg>
        </button>
      </div>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.driven-toast-enter-active,
.driven-toast-leave-active {
  transition:
    opacity 200ms ease,
    transform 200ms ease;
}

.driven-toast-enter-from,
.driven-toast-leave-to {
  opacity: 0;
  transform: translateX(1rem);
}

/* A leaving toast is taken out of flow so the ones above it slide down smoothly
   instead of jumping the instant it is removed. */
.driven-toast-leave-active {
  position: absolute;
  right: 0;
  left: 0;
}

.driven-toast-move {
  transition: transform 200ms ease;
}

/* Reduced motion: keep the fade (it still marks arrival/departure) but drop the
   slide and the reflow animation entirely. */
@media (prefers-reduced-motion: reduce) {
  .driven-toast-enter-from,
  .driven-toast-leave-to {
    transform: none;
  }

  .driven-toast-enter-active,
  .driven-toast-leave-active,
  .driven-toast-move {
    transition: opacity 200ms ease;
  }
}
</style>
