<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import { sanitizeMarkdown } from "./sanitizeMarkdown";
import type { ReleaseDto } from "../ipc/types";

// ChangelogModal (SPEC s15 / ROADMAP M9): renders one release's notes (markdown
// body) for the available version, opened from the update banner / About tab.
// The markdown is rendered through `sanitizeMarkdown` (HTML-escaped first, then a
// small whitelist of tags) so a malicious release body can never inject raw HTML
// / scripts into the webview. All chrome strings are i18n via t() (no raw English
// in the template).
const { t, locale } = useI18n();

const props = defineProps<{ release: ReleaseDto | null }>();
const emit = defineEmits<{ close: [] }>();

const open = computed(() => props.release !== null);

/** The release notes rendered to sanitized HTML (empty string when no notes). */
const renderedNotes = computed(() =>
  props.release ? sanitizeMarkdown(props.release.notes ?? "") : "",
);

/** A localized publish date, falling back to the raw string if unparseable. */
const formattedDate = computed(() => {
  if (!props.release || !props.release.publishedAt) return "";
  const date = new Date(props.release.publishedAt);
  if (Number.isNaN(date.getTime())) return props.release.publishedAt;
  return new Intl.DateTimeFormat(locale.value, { dateStyle: "medium" }).format(
    date,
  );
});

function close(): void {
  emit("close");
}
</script>

<template>
  <div
    v-if="open"
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
    role="dialog"
    aria-modal="true"
    data-testid="changelog-modal"
    @click.self="close"
  >
    <div
      class="max-h-[80vh] w-full max-w-lg overflow-y-auto rounded-lg bg-white p-6 shadow-xl dark:bg-zinc-900"
    >
      <div class="mb-4 flex items-start justify-between gap-4">
        <div>
          <h2 class="text-lg font-semibold">
            {{ t("changelog.title", { version: release?.name ?? release?.version }) }}
          </h2>
          <p
            v-if="formattedDate"
            class="text-xs text-zinc-400"
          >
            {{ formattedDate }}
          </p>
        </div>
        <button
          type="button"
          class="rounded px-2 py-1 text-sm text-zinc-500 hover:text-zinc-800 dark:hover:text-zinc-200"
          :aria-label="t('common.close')"
          @click="close"
        >
          {{ t("common.close") }}
        </button>
      </div>

      <!-- eslint-disable vue/no-v-html -- content is sanitized by sanitizeMarkdown (HTML-escaped first, whitelist of tags only). -->
      <div
        v-if="renderedNotes"
        class="prose-sm space-y-2 text-sm text-zinc-700 dark:text-zinc-300"
        data-testid="changelog-body"
        v-html="renderedNotes"
      />
      <!-- eslint-enable vue/no-v-html -->
      <p
        v-else
        class="text-sm text-zinc-500"
        data-testid="changelog-empty"
      >
        {{ t("changelog.noNotes") }}
      </p>
    </div>
  </div>
</template>
