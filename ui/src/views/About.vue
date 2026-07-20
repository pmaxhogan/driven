<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { getVersion } from "@tauri-apps/api/app";

import * as ipc from "../ipc/commands";
import { useSettingsStore } from "../stores/settings";
import { useUpdaterStore } from "../stores/updater";
import ChangelogModal from "../components/ChangelogModal.vue";
import TelemetryPreviewModal from "../components/TelemetryPreviewModal.vue";
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

// Shared design-system class strings (DRIVEN UI design system). Teal is the
// accent for primary/interactive affordances; native controls carry explicit
// light/dark surfaces so they stay readable on a dark-theme OS.
const primaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const secondaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const inputCls =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-hidden focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const cardCls =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-xs dark:border-zinc-800 dark:bg-zinc-900";

// R1-P2-1: the V1 macOS in-app updater is not expected to work cleanly, so on
// macOS we hide the in-app "Install update" action and surface a manual DMG
// download link to the GitHub releases page instead. Windows + Linux keep the
// in-app install.
const isMac = isMacOS();
// The GitHub releases page the macOS user downloads the DMG from.
// R9-P2-3 / dev-channel floor: derive this from the offered VERSION SHAPE, not the
// endpoint channel. A `-dev` prerelease build lives on the rolling dev pre-release
// (`/releases/tag/dev`, the tag dev-channel.yml publishes). Any clean release -
// INCLUDING a dev-channel manifest that was FLOORED up to stable
// (docs/superpowers/specs/2026-06-25-dev-channel-floor-design.md), whose assets
// live on the stable tag and NOT on the rolling dev release - must point at the
// stable latest release. Keying off the channel would send a floored dev user to
// `/releases/tag/dev`, which lacks the offered stable assets. Default to the
// stable latest page when no update is in hand (e.g. before any check).
const releasesBase = "https://github.com/pmaxhogan/driven/releases";
const macDownloadUrl = computed(() =>
  (updater.available?.version ?? "").includes("-dev")
    ? `${releasesBase}/tag/dev`
    : `${releasesBase}/latest`
);

const exporting = ref(false);
const exportError = ref<string | null>(null);
const exportedPath = ref<string | null>(null);

const telemetryEnabled = computed(() => settings.settings?.telemetry.enabled ?? false);

// SPEC s16 telemetry preview (#34): shows the exact next-ping JSON payload in a
// modal, available regardless of the current enabled state - a privacy-
// conscious user inspects it BEFORE opting in. Mirrors the Settings-tab link.
const showTelemetryPreview = ref(false);

function formatReleaseDate(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return new Intl.DateTimeFormat(locale.value, { dateStyle: "medium" }).format(date);
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
  // SPEC s16 (M9b R2-P1-1): use the dedicated set_telemetry_enabled command so the
  // backend flips the in-flight ping cancel flag immediately (opt-out honored mid-ping).
  await settings.setTelemetryEnabled(checkedValue);
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
  <section class="max-w-2xl space-y-6">
    <h1 class="text-2xl font-semibold">
      {{ t("about.title") }}
    </h1>

    <!-- Update-available banner (listens to updater:available via the store). An
         available update is actionable, not a success state, so it uses the teal
         accent (emerald is reserved for "done" status). -->
    <div
      v-if="updater.bannerVisible && updater.available"
      class="space-y-3 rounded-lg border border-teal-300 bg-teal-50 p-4 text-sm dark:border-teal-800 dark:bg-teal-950"
      data-testid="update-banner"
    >
      <p class="font-medium text-teal-800 dark:text-teal-200">
        {{ t("about.updateAvailable", { version: updater.available.version }) }}
      </p>
      <!-- macOS: the V1 in-app updater is not reliable, so offer a manual DMG
           download instead of in-app install (R1-P2-1). -->
      <p
        v-if="isMac"
        class="text-sm text-zinc-600 dark:text-zinc-300"
        data-testid="install-mac-unsupported"
      >
        {{ t("about.installUpdateMacUnsupported") }}
      </p>
      <div class="flex flex-wrap items-center gap-2">
        <a
          v-if="isMac"
          :href="macDownloadUrl"
          target="_blank"
          rel="noopener noreferrer"
          :class="primaryBtn"
          data-testid="download-latest-dmg"
        >
          {{ t("about.downloadLatestDmgButton") }}
        </a>
        <button
          v-else
          type="button"
          :class="primaryBtn"
          :disabled="updater.installing"
          data-testid="install-update"
          @click="updater.install()"
        >
          {{ t("about.installUpdateButton") }}
        </button>
        <button type="button" :class="secondaryBtn" @click="updater.openAvailableChangelog()">
          {{ t("about.viewChangelogButton") }}
        </button>
        <button
          type="button"
          class="rounded-md px-3 py-2 text-sm text-zinc-500 transition-colors hover:text-zinc-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:hover:text-zinc-300"
          @click="updater.dismissBanner()"
        >
          {{ t("common.close") }}
        </button>
      </div>

      <!-- Download progress while installing (Windows/Linux only). -->
      <div v-if="!isMac && updater.installing" class="space-y-1" data-testid="install-progress">
        <div class="h-2 w-full overflow-hidden rounded-sm bg-teal-200 dark:bg-teal-900">
          <div
            class="h-full bg-teal-600 transition-all"
            :style="{
              width:
                updater.downloadFraction !== null
                  ? `${Math.round(updater.downloadFraction * 100)}%`
                  : '100%',
            }"
          />
        </div>
        <p class="text-xs text-zinc-600 dark:text-zinc-400">
          {{ updater.downloadComplete ? t("about.updateDownloaded") : t("about.downloading") }}
        </p>
      </div>
      <p v-if="updater.installErrorCode" class="text-sm text-red-600" data-testid="install-error">
        {{ localizeError(updater.installErrorCode) }}
      </p>
    </div>

    <!-- App version + license -->
    <div class="space-y-1 text-sm" :class="cardCls">
      <p>{{ t("about.version", { version }) }}</p>
      <p>
        <span class="text-zinc-500">{{ t("about.licenseLabel") }}:</span>
        {{ license }}
      </p>
    </div>

    <!-- Updates: channel + check action + status -->
    <div class="space-y-3" :class="cardCls">
      <h2 class="text-lg font-medium">
        {{ t("about.updatesTitle") }}
      </h2>
      <label class="block max-w-xs space-y-1 text-sm">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("about.channelLabel") }}</span>
        <select
          :value="updater.channel"
          class="w-full"
          :class="inputCls"
          data-testid="channel-select"
          @change="onChannelChange"
        >
          <option v-for="ch in channels" :key="ch" :value="ch">
            {{ t(`about.channel.${ch}`) }}
          </option>
        </select>
      </label>

      <div class="space-y-2">
        <button
          type="button"
          :class="primaryBtn"
          :disabled="updater.checking"
          data-testid="check-updates"
          @click="updater.check()"
        >
          {{ t("about.checkForUpdatesButton") }}
        </button>
        <p v-if="updater.checking" class="text-sm text-zinc-500">
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
        <p v-else-if="updater.checked" class="text-sm text-zinc-500" data-testid="check-uptodate">
          {{ t("about.upToDate") }}
        </p>
      </div>
    </div>

    <!-- Privacy: telemetry opt-out + display language -->
    <div class="space-y-3" :class="cardCls">
      <h2 class="text-lg font-medium">
        {{ t("about.privacyTitle") }}
      </h2>
      <label class="flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          class="accent-teal-600"
          :checked="telemetryEnabled"
          @change="setTelemetry"
        />
        {{ t("about.telemetryLabel") }}
      </label>
      <button
        type="button"
        class="rounded-xs text-xs font-medium text-teal-700 underline transition-colors hover:text-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300"
        data-testid="telemetry-preview-open"
        @click="showTelemetryPreview = true"
      >
        {{ t("settings.rules.telemetryPreviewButton") }}
      </button>
      <p class="text-sm text-zinc-500">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("about.displayLanguageLabel") }}:</span>
        {{ t("about.moreLanguagesComing") }}
      </p>
    </div>

    <!-- Diagnostics: export bundle -->
    <div class="space-y-2" :class="cardCls">
      <h2 class="text-lg font-medium">
        {{ t("about.diagnosticsTitle") }}
      </h2>
      <button type="button" :class="secondaryBtn" :disabled="exporting" @click="exportDiagnostics">
        {{ t("about.exportDiagnosticsButton") }}
      </button>
      <p v-if="exportError" class="text-sm text-red-600">
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

    <!-- Release notes -->
    <div class="space-y-3" :class="cardCls">
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
      <ul v-else-if="updater.releases.length > 0" class="space-y-3" data-testid="release-notes">
        <li
          v-for="release in updater.releases"
          :key="release.version"
          class="space-y-1 border-b border-zinc-200 pb-3 dark:border-zinc-800"
        >
          <p class="text-sm font-medium">
            {{ release.name }}
          </p>
          <p class="text-xs text-zinc-400">
            {{ formatReleaseDate(release.publishedAt) }}
          </p>
          <button
            type="button"
            class="rounded-xs text-sm font-medium text-teal-700 underline transition-colors hover:text-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300"
            @click="viewReleaseChangelog(release)"
          >
            {{ t("about.viewChangelogButton") }}
          </button>
        </li>
      </ul>
      <button
        v-if="updater.hasMoreReleases"
        type="button"
        :class="secondaryBtn"
        :disabled="updater.releasesLoading"
        data-testid="load-more-releases"
        @click="updater.loadMoreReleases()"
      >
        {{ t("about.loadMoreReleasesButton") }}
      </button>
    </div>

    <ChangelogModal :release="updater.changelogRelease" @close="updater.closeChangelog()" />
    <TelemetryPreviewModal :open="showTelemetryPreview" @close="showTelemetryPreview = false" />
  </section>
</template>
