<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { getVersion } from "@tauri-apps/api/app";

import * as ipc from "../ipc/commands";
import { useSettingsStore } from "../stores/settings";
import type { ReleaseDto, UpdateInfo } from "../ipc/types";

// About view (SPEC s11.6, s25 /about; DESIGN s8.2 About tab). Shows the app
// version, the update channel selector (stable / dev), Check-for-updates, the
// license, the release-notes viewer (list_releases), the telemetry opt-out, the
// diagnostic-bundle export, and a "more languages coming" placeholder (DESIGN
// s8.7: V1 ships en-US only, the selector arrives with V2). Channel + telemetry
// edits round-trip through the settings store; the version comes from the Tauri
// app metadata; the license is the workspace SPDX id (SPEC s23).
const { t, locale } = useI18n();
const settings = useSettingsStore();

const version = ref<string>("");
// Workspace license (SPEC s23 workspace.package.license). Bound via a ref so it
// renders through an interpolation (an SPDX id is not a translatable string).
const license = ref<string>("MIT OR Apache-2.0");

const channels = ["stable", "dev"] as const;

const checking = ref(false);
const checked = ref(false);
const update = ref<UpdateInfo | null>(null);
const checkError = ref<string | null>(null);

const releases = ref<ReleaseDto[]>([]);
const releasesLoading = ref(false);
const releasesError = ref<string | null>(null);

const exporting = ref(false);
const exportError = ref<string | null>(null);
const exportedPath = ref<string | null>(null);

const channel = computed(() => settings.settings?.updater.channel ?? "stable");
const telemetryEnabled = computed(
  () => settings.settings?.telemetry.enabled ?? false,
);

function formatReleaseDate(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return new Intl.DateTimeFormat(locale.value, { dateStyle: "medium" }).format(
    date,
  );
}

onMounted(async () => {
  try {
    version.value = await getVersion();
  } catch {
    version.value = "";
  }
  if (settings.settings === null) {
    await settings.refresh();
  }
  await loadReleases();
});

async function checkForUpdates(): Promise<void> {
  checking.value = true;
  checkError.value = null;
  try {
    update.value = await ipc.checkForUpdates();
    checked.value = true;
  } catch (e) {
    checkError.value = String(e);
  } finally {
    checking.value = false;
  }
}

async function setChannel(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await settings.patch({ updater: { channel: value } });
  // A channel change can change what "latest" means; refresh the notes.
  await loadReleases();
}

async function setTelemetry(event: Event): Promise<void> {
  const checkedValue = (event.target as HTMLInputElement).checked;
  await settings.patch({ telemetry: { enabled: checkedValue } });
}

async function loadReleases(): Promise<void> {
  releasesLoading.value = true;
  releasesError.value = null;
  try {
    releases.value = await ipc.listReleases(1);
  } catch (e) {
    releasesError.value = String(e);
  } finally {
    releasesLoading.value = false;
  }
}

async function exportDiagnostics(): Promise<void> {
  exportError.value = null;
  exportedPath.value = null;
  // C1/C2: the BACKEND owns the save-file dialog and returns a concrete `.zip`
  // path + a one-shot token. The webview never supplies a write target; the
  // backend writes the ZIP at the token-bound path (SPEC s11.6.1).
  let token: string;
  try {
    const picked = await ipc.pickSaveZipDialog();
    token = picked.token;
  } catch {
    // Cancel (or dialog error): nothing to export.
    return;
  }
  exporting.value = true;
  try {
    exportedPath.value = await ipc.exportDiagnosticBundle(token);
  } catch (e) {
    exportError.value = String(e);
  } finally {
    exporting.value = false;
  }
}
</script>

<template>
  <section class="space-y-6">
    <h1 class="text-2xl font-semibold">
      {{ t("about.title") }}
    </h1>

    <div class="space-y-1 text-sm">
      <p>{{ t("about.version", { version }) }}</p>
      <p>
        <span class="text-zinc-500">{{ t("about.licenseLabel") }}:</span>
        {{ license }}
      </p>
    </div>

    <div class="max-w-xs space-y-1 text-sm">
      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("about.channelLabel")
        }}</span>
        <select
          :value="channel"
          class="w-full rounded border px-2 py-1"
          @change="setChannel"
        >
          <option
            v-for="ch in channels"
            :key="ch"
            :value="ch"
          >
            {{ t(`about.channel.${ch}`) }}
          </option>
        </select>
      </label>
    </div>

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
        v-else-if="checkError"
        class="text-sm text-red-600"
      >
        {{ checkError }}
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

    <label class="flex max-w-md items-center gap-2 text-sm">
      <input
        type="checkbox"
        :checked="telemetryEnabled"
        @change="setTelemetry"
      >
      {{ t("about.telemetryLabel") }}
    </label>

    <div class="space-y-2">
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="exporting"
        @click="exportDiagnostics"
      >
        {{ t("about.exportDiagnosticsButton") }}
      </button>
      <p
        v-if="exportError"
        class="text-sm text-red-600"
      >
        {{ exportError }}
      </p>
      <p
        v-else-if="exportedPath"
        class="break-all text-sm text-zinc-600 dark:text-zinc-400"
        data-testid="exported-path"
      >
        {{ exportedPath }}
      </p>
    </div>

    <div class="space-y-2">
      <h2 class="text-lg font-medium">
        {{ t("about.releaseNotesTitle") }}
      </h2>
      <p
        v-if="releasesLoading"
        class="text-sm text-zinc-500"
      >
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="releasesError"
        class="text-sm text-red-600"
      >
        {{ releasesError }}
      </p>
      <ul
        v-else-if="releases.length > 0"
        class="space-y-3"
        data-testid="release-notes"
      >
        <li
          v-for="release in releases"
          :key="release.version"
          class="space-y-1 border-b pb-3"
        >
          <p class="text-sm font-medium">
            {{ release.name }}
          </p>
          <p class="text-xs text-zinc-400">
            {{ formatReleaseDate(release.publishedAt) }}
          </p>
          <p class="whitespace-pre-line text-sm text-zinc-600 dark:text-zinc-400">
            {{ release.notes }}
          </p>
        </li>
      </ul>
    </div>

    <p class="text-sm text-zinc-500">
      <span class="text-zinc-600 dark:text-zinc-400">{{
        t("about.displayLanguageLabel")
      }}:</span>
      {{ t("about.moreLanguagesComing") }}
    </p>
  </section>
</template>
