<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { getVersion } from "@tauri-apps/api/app";

import * as ipc from "../ipc/commands";
import { useSettingsStore } from "../stores/settings";
import { useUpdaterStore } from "../stores/updater";
import ChangelogModal from "../components/ChangelogModal.vue";
import { isMacOS } from "../platform";
import type { ReleaseDto } from "../ipc/types";

// About view (SPEC s11.6, s15, s25 /about; DESIGN s8.2 About tab). Shows the app
// version, the update channel selector (stable / dev), Check-for-updates, an
// in-app "update available" banner with Install + download progress + View
// changelog, the paginated release-notes viewer (list_releases) with a per-entry
// ChangelogModal, the license, the telemetry opt-out, and the diagnostic-bundle
// export. Channel + the updater flow round-trip through the updater store; the
// telemetry toggle + diagnostics through the settings store; the version comes
// from the Tauri app metadata; the license is the workspace SPDX id (SPEC s23).
const { t, locale } = useI18n();
const settings = useSettingsStore();
const updater = useUpdaterStore();

const version = ref<string>("");
// Workspace license (SPEC s23 workspace.package.license).
const license = ref<string>("MIT OR Apache-2.0");

const channels = ["stable", "dev"] as const;

// R1-P2-1: the V1 macOS in-app updater is not expected to work cleanly, so on
// macOS we hide the in-app "Install update" action and surface a manual DMG
// download link to the GitHub releases page instead. Windows + Linux keep the
// in-app install.
const isMac = isMacOS();
// The GitHub releases page the macOS user downloads the latest DMG from.
const latestReleaseUrl = "https://github.com/pmaxhogan/driven/releases/latest";

const exporting = ref(false);
const exportError = ref<string | null>(null);
const exportedPath = ref<string | null>(null);

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

/** Localize a SPEC s24 error code, falling back to a generic message. */
function localizeError(code: string | null): string {
  if (code === null) return "";
  return t(`errors.${code}.long`);
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
  // R2-P1-3: the updater EVENT subscriptions are owned at the app root (App.vue)
  // so a startup `updater:available` is never lost when About is not mounted.
  // About only loads the channel + releases list for its own surface; the banner
  // is driven entirely by the shared store state the root subscription feeds.
  await updater.loadChannel();
  await updater.loadReleases();
});

async function onChannelChange(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await updater.setChannel(value);
}

async function setTelemetry(event: Event): Promise<void> {
  const checkedValue = (event.target as HTMLInputElement).checked;
  await settings.patch({ telemetry: { enabled: checkedValue } });
}

async function exportDiagnostics(): Promise<void> {
  exportError.value = null;
  exportedPath.value = null;
  // C1/C2: the BACKEND owns the save-file dialog and returns a concrete `.zip`
  // path + a one-shot token. The webview never supplies a write target.
  let token: string;
  try {
    const picked = await ipc.pickSaveZipDialog();
    token = picked.token;
  } catch {
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

function viewReleaseChangelog(release: ReleaseDto): void {
  updater.openChangelog(release);
}
</script>

<template>
  <section class="space-y-6">
    <h1 class="text-2xl font-semibold">
      {{ t("about.title") }}
    </h1>

    <!-- Update-available banner (listens to updater:available via the store). -->
    <div
      v-if="updater.bannerVisible && updater.available"
      class="space-y-2 rounded border border-emerald-300 bg-emerald-50 p-4 text-sm dark:border-emerald-700 dark:bg-emerald-950"
      data-testid="update-banner"
    >
      <p class="font-medium">
        {{ t("about.updateAvailable", { version: updater.available.version }) }}
      </p>
      <!-- macOS: the V1 in-app updater is not reliable, so offer a manual DMG
           download instead of in-app install (R1-P2-1). -->
      <p
        v-if="isMac"
        class="text-sm"
        data-testid="install-mac-unsupported"
      >
        {{ t("about.installUpdateMacUnsupported") }}
      </p>
      <div class="flex flex-wrap items-center gap-2">
        <a
          v-if="isMac"
          :href="latestReleaseUrl"
          target="_blank"
          rel="noopener noreferrer"
          class="rounded bg-emerald-600 px-3 py-1.5 text-white"
          data-testid="download-latest-dmg"
        >
          {{ t("about.downloadLatestDmgButton") }}
        </a>
        <button
          v-else
          type="button"
          class="rounded bg-emerald-600 px-3 py-1.5 text-white disabled:opacity-50"
          :disabled="updater.installing"
          data-testid="install-update"
          @click="updater.install()"
        >
          {{ t("about.installUpdateButton") }}
        </button>
        <button
          type="button"
          class="rounded border px-3 py-1.5"
          @click="updater.openAvailableChangelog()"
        >
          {{ t("about.viewChangelogButton") }}
        </button>
        <button
          type="button"
          class="rounded px-3 py-1.5 text-zinc-500"
          @click="updater.dismissBanner()"
        >
          {{ t("common.close") }}
        </button>
      </div>

      <!-- Download progress while installing (Windows/Linux only). -->
      <div
        v-if="!isMac && updater.installing"
        class="space-y-1"
        data-testid="install-progress"
      >
        <div class="h-2 w-full overflow-hidden rounded bg-emerald-200 dark:bg-emerald-900">
          <div
            class="h-full bg-emerald-600 transition-all"
            :style="{
              width:
                updater.downloadFraction !== null
                  ? `${Math.round(updater.downloadFraction * 100)}%`
                  : '100%',
            }"
          />
        </div>
        <p class="text-xs text-zinc-600 dark:text-zinc-400">
          {{
            updater.downloadComplete
              ? t("about.updateDownloaded")
              : t("about.downloading")
          }}
        </p>
      </div>
      <p
        v-if="updater.installErrorCode"
        class="text-sm text-red-600"
        data-testid="install-error"
      >
        {{ localizeError(updater.installErrorCode) }}
      </p>
    </div>

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
          :value="updater.channel"
          class="w-full rounded border px-2 py-1"
          data-testid="channel-select"
          @change="onChannelChange"
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
        :disabled="updater.checking"
        data-testid="check-updates"
        @click="updater.check()"
      >
        {{ t("about.checkForUpdatesButton") }}
      </button>
      <p
        v-if="updater.checking"
        class="text-sm text-zinc-500"
      >
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="updater.checkErrorCode"
        class="text-sm text-red-600"
        data-testid="check-error"
      >
        {{ localizeError(updater.checkErrorCode) }}
      </p>
      <p
        v-else-if="updater.checked && updater.available"
        class="text-sm"
        data-testid="check-available"
      >
        {{ t("about.updateAvailable", { version: updater.available.version }) }}
      </p>
      <p
        v-else-if="updater.checked"
        class="text-sm text-zinc-500"
        data-testid="check-uptodate"
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
        v-if="updater.releasesLoading && updater.releases.length === 0"
        class="text-sm text-zinc-500"
      >
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="updater.releasesErrorCode"
        class="text-sm text-red-600"
        data-testid="releases-error"
      >
        {{ localizeError(updater.releasesErrorCode) }}
      </p>
      <ul
        v-else-if="updater.releases.length > 0"
        class="space-y-3"
        data-testid="release-notes"
      >
        <li
          v-for="release in updater.releases"
          :key="release.version"
          class="space-y-1 border-b pb-3"
        >
          <p class="text-sm font-medium">
            {{ release.name }}
          </p>
          <p class="text-xs text-zinc-400">
            {{ formatReleaseDate(release.publishedAt) }}
          </p>
          <button
            type="button"
            class="text-sm text-emerald-700 underline dark:text-emerald-400"
            @click="viewReleaseChangelog(release)"
          >
            {{ t("about.viewChangelogButton") }}
          </button>
        </li>
      </ul>
      <button
        v-if="updater.hasMoreReleases"
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="updater.releasesLoading"
        data-testid="load-more-releases"
        @click="updater.loadMoreReleases()"
      >
        {{ t("about.loadMoreReleasesButton") }}
      </button>
    </div>

    <p class="text-sm text-zinc-500">
      <span class="text-zinc-600 dark:text-zinc-400">{{
        t("about.displayLanguageLabel")
      }}:</span>
      {{ t("about.moreLanguagesComing") }}
    </p>

    <ChangelogModal
      :release="updater.changelogRelease"
      @close="updater.closeChangelog()"
    />
  </section>
</template>
