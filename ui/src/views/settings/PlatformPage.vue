<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { getApfsHelperStatus, getVssHelperStatus } from "../../ipc/commands";
import type { ApfsHelperStatus, MenuBarSettingsPatch, VssHelperStatus } from "../../ipc/types";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// Platform settings page (SDD 2026-08-02 settings-sidebar-ia, task 2). Moved
// verbatim out of Settings.vue: the "Startup" card (autoStartOnLogin), the
// Windows VSS / macOS APFS-snapshot locked-file backup controls that used to
// live inline inside the old "Performance and bandwidth" card, and the
// macOS-only "Menu bar extra" card. Rendered from the existing
// `settings.windows` / `settings.macos` nullability - label is "macOS" or
// "Windows" and the page is hidden from the nav when both are null (Linux);
// that nav-hiding lands in task 3, this task only carries the moved markup.
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch } = useSettingsForm();

ensureSettingsLoaded();

const vssModes = ["auto", "always", "never"] as const;

// Least-privilege locked-file backup status (DESIGN s5.3.1). Drives the
// banner shown when locked files (Outlook / DB / VM disks) are being skipped
// because Volume Shadow Copy is unavailable (no elevation, no active helper).
const vssStatus = ref<VssHelperStatus | null>(null);
const showVssBanner = computed(() => vssStatus.value?.lockedFileBackupDegraded ?? false);
// Issue #25 (launch UX): while the helper is launching (awaiting UAC), show a
// "waiting for approval" hint; once the user declines, show a "declined" hint.
const showVssPending = computed(() => vssStatus.value?.launchPending ?? false);
const showVssDeclined = computed(() => vssStatus.value?.launchDeclined ?? false);

// Poll timer used to watch a pending launch resolve (pending -> ready/declined).
// Bounded + cleared on unmount so no timer is orphaned.
let vssPollTimer: ReturnType<typeof setTimeout> | null = null;
function clearVssPoll(): void {
  if (vssPollTimer !== null) {
    clearTimeout(vssPollTimer);
    vssPollTimer = null;
  }
}
async function refreshVssStatus(): Promise<void> {
  try {
    vssStatus.value = await getVssHelperStatus();
  } catch {
    vssStatus.value = null;
  }
}
// After the eager enable, the elevated launch resolves asynchronously; re-fetch
// the status a few times so the page reflects pending -> ready/declined
// without a manual refresh.
function pollVssStatusWhilePending(attemptsLeft: number): void {
  clearVssPoll();
  if (attemptsLeft <= 0) return;
  vssPollTimer = setTimeout(() => {
    void refreshVssStatus().then(() => {
      if (vssStatus.value?.launchPending) {
        pollVssStatusWhilePending(attemptsLeft - 1);
      }
    });
  }, 1500);
}
onUnmounted(clearVssPoll);

// macOS locked-file backup status (DESIGN s5.3.2). The APFS-snapshot path is the
// mac twin of the Windows VSS helper above: a small privileged helper mounts a
// read-only local snapshot so held-open files can be read. Kept as its own set of
// refs/functions rather than generalised over the VSS ones - the two platforms'
// statuses are independent and only one of them is ever non-null.
const apfsStatus = ref<ApfsHelperStatus | null>(null);
// While the helper is being installed/launched, show a "waiting for approval"
// hint; once the user declines the administrator prompt, show a "declined" hint.
const showApfsPending = computed(() => apfsStatus.value?.launchPending ?? false);
const showApfsDeclined = computed(() => apfsStatus.value?.launchDeclined ?? false);

// Poll timer used to watch a pending launch resolve (pending -> ready/declined).
// Bounded + cleared on unmount so no timer is orphaned.
let apfsPollTimer: ReturnType<typeof setTimeout> | null = null;
function clearApfsPoll(): void {
  if (apfsPollTimer !== null) {
    clearTimeout(apfsPollTimer);
    apfsPollTimer = null;
  }
}
async function refreshApfsStatus(): Promise<void> {
  try {
    apfsStatus.value = await getApfsHelperStatus();
  } catch {
    apfsStatus.value = null;
  }
}
// After the eager enable, the privileged launch resolves asynchronously; re-fetch
// the status a few times so the page reflects pending -> ready/declined
// without a manual refresh.
function pollApfsStatusWhilePending(attemptsLeft: number): void {
  clearApfsPoll();
  if (attemptsLeft <= 0) return;
  apfsPollTimer = setTimeout(() => {
    void refreshApfsStatus().then(() => {
      if (apfsStatus.value?.launchPending) {
        pollApfsStatusWhilePending(attemptsLeft - 1);
      }
    });
  }, 1500);
}
onUnmounted(clearApfsPoll);

// Fetch both platform statuses as soon as this page mounts - equivalent to
// the old `watch(active, ..., { immediate: true })` in Settings.vue that ran
// whenever the Rules tab became active, now scoped to this page's own
// lifecycle since PlatformPage is mounted exactly when the tab is.
onMounted(() => {
  void getVssHelperStatus()
    .then((s) => (vssStatus.value = s))
    .catch(() => {
      // No status (e.g. IPC unavailable in a browser dev shell): hide the
      // banner rather than surface an error on the page.
      vssStatus.value = null;
    });
  void getApfsHelperStatus()
    .then((s) => (apfsStatus.value = s))
    .catch(() => {
      // Same fallback as the VSS status above: no status = no hints.
      apfsStatus.value = null;
    });
});

// Issue #58: launch Driven at login (default ON). Patches the persisted
// preference; the backend registers/unregisters the real OS startup entry.
async function setAutoStartOnLogin(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { autoStartOnLogin: checked } });
}

async function setVssMode(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await commitPatch({ windows: { vssMode: value } });
}

// Issue #25 (DESIGN s5.3.1): toggle the least-privilege VSS helper. When on (and
// the app is not elevated), enabling fires the ATTENDED elevation prompt right
// away (the user is here to approve it); the launch resolves asynchronously, so
// we re-fetch the status and poll while it is pending to show waiting -> ready /
// declined without a manual refresh.
async function setVssHelper(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ windows: { vssHelper: checked } });
  await refreshVssStatus();
  if (checked && vssStatus.value?.launchPending) {
    // Watch the pending launch resolve (bounded; cleared on unmount).
    pollVssStatusWhilePending(60);
  } else {
    clearVssPoll();
  }
}

// DESIGN s5.3.2: toggle APFS-snapshot backup of locked files on macOS. Enabling
// it fires the ATTENDED administrator prompt right away (the user is here to
// approve it); the launch resolves asynchronously, so we re-fetch the status and
// poll while it is pending to show waiting -> ready / declined without a manual
// refresh.
async function setApfsSnapshot(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ macos: { apfsSnapshot: checked } });
  await refreshApfsStatus();
  if (checked && apfsStatus.value?.launchPending) {
    // Watch the pending launch resolve (bounded; cleared on unmount).
    pollApfsStatusWhilePending(60);
  } else {
    clearApfsPoll();
  }
}

// Menu bar extra (spec 2026-07-31 s2): each control patches exactly the one
// changed `macos.menuBar` field - the backend merges field-wise, so sending
// the whole object would clobber concurrent edits to the others.
const menuBarIdleOptions = ["none", "lastBackupAge", "uploadedToday"] as const;
const menuBarIdleLabelKeys: Record<(typeof menuBarIdleOptions)[number], string> = {
  none: "settings.rules.menuBarIdleNone",
  lastBackupAge: "settings.rules.menuBarIdleLastBackup",
  uploadedToday: "settings.rules.menuBarIdleUploadedToday",
};
function menuBarIdleLabelKey(idle: (typeof menuBarIdleOptions)[number]): string {
  return menuBarIdleLabelKeys[idle];
}

async function patchMenuBar(
  field: keyof MenuBarSettingsPatch,
  value: boolean | string
): Promise<void> {
  await commitPatch({ macos: { menuBar: { [field]: value } } });
}

async function setMenuBarSpeed(event: Event): Promise<void> {
  await patchMenuBar("showUploadSpeed", (event.target as HTMLInputElement).checked);
}

async function setMenuBarPercent(event: Event): Promise<void> {
  await patchMenuBar("showPercent", (event.target as HTMLInputElement).checked);
}

async function setMenuBarFiles(event: Event): Promise<void> {
  await patchMenuBar("showFiles", (event.target as HTMLInputElement).checked);
}

async function setMenuBarEta(event: Event): Promise<void> {
  await patchMenuBar("showEta", (event.target as HTMLInputElement).checked);
}

async function setMenuBarIdle(event: Event): Promise<void> {
  await patchMenuBar("idle", (event.target as HTMLSelectElement).value);
}

// Static preview strip (DESIGN spec 2026-07-31 s2): sample metrics fixed at
// build time, formatted with the SAME field order + " · " separator as the
// backend's `format_title` (src-tauri/src/menubar.rs) so the preview never
// drifts from what actually renders in the real menu bar.
const SAMPLE_PERCENT = "62%";
const SAMPLE_SPEED = "84 Mbps";
const SAMPLE_FILES = "341/2.1k";
const SAMPLE_ETA = "~4m";

const sampleTitle = computed(() => {
  const menuBar = settings.settings?.macos?.menuBar;
  if (!menuBar) return "";
  const parts: string[] = [];
  if (menuBar.showPercent) parts.push(SAMPLE_PERCENT);
  if (menuBar.showUploadSpeed) parts.push(SAMPLE_SPEED);
  if (menuBar.showFiles) parts.push(SAMPLE_FILES);
  if (menuBar.showEta) parts.push(SAMPLE_ETA);
  return parts.join(" \u{b7} ");
});

const menuBarEnabledCount = computed(() => {
  const menuBar = settings.settings?.macos?.menuBar;
  if (!menuBar) return 0;
  return [menuBar.showUploadSpeed, menuBar.showPercent, menuBar.showFiles, menuBar.showEta].filter(
    Boolean
  ).length;
});
</script>

<template>
  <p v-if="settings.loading && !settings.settings" class="text-sm text-zinc-500">
    {{ t("common.loading") }}
  </p>
  <p v-else-if="!settings.settings && settings.errorCode" class="text-sm text-red-600" role="alert">
    {{ t(`errors.${settings.errorCode}.long`) }}
  </p>
  <div v-else-if="settings.settings" class="max-w-2xl space-y-4 text-sm" data-testid="rules-form">
    <p
      v-if="settings.errorCode"
      class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-950/40 dark:text-red-300"
      role="alert"
      data-testid="rules-error"
    >
      {{ t(`errors.${settings.errorCode}.long`) }}
    </p>

    <!-- Startup -->
    <section class="space-y-3" :class="cardCls" data-testid="startup-setting">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.sections.startup") }}
      </h3>
      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="autostart-toggle"
          :checked="settings.settings.global.autoStartOnLogin"
          @change="setAutoStartOnLogin"
        />
        {{ t("settings.rules.autoStartOnLoginLabel") }}
      </label>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("settings.rules.autoStartOnLoginNote") }}
      </p>
    </section>

    <!-- Locked-file backup: Windows VSS / macOS APFS snapshot -->
    <section class="space-y-3" :class="cardCls">
      <div
        v-if="showVssBanner"
        data-testid="vss-degraded-banner"
        class="rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800/60 dark:bg-amber-950/40"
      >
        <h4 class="font-semibold text-amber-800 dark:text-amber-300">
          {{ t("settings.rules.vssBanner.title") }}
        </h4>
        <p class="mt-1 text-amber-700 dark:text-amber-200/80">
          {{ t("settings.rules.vssBanner.body") }}
        </p>
      </div>

      <label v-if="settings.settings.windows" class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("settings.rules.vssModeLabel") }}</span>
        <select
          data-testid="vss-mode"
          class="w-full"
          :class="inputCls"
          :value="settings.settings.windows.vssMode"
          @change="setVssMode"
        >
          <option v-for="mode in vssModes" :key="mode" :value="mode">
            {{ t(`settings.rules.vssMode.${mode}`) }}
          </option>
        </select>
      </label>

      <!-- Issue #25: least-privilege VSS helper toggle (DESIGN s5.3.1). -->
      <label
        v-if="settings.settings.windows"
        class="block space-y-1"
        data-testid="vss-helper-setting"
      >
        <span class="flex items-center gap-2">
          <input
            type="checkbox"
            class="accent-teal-600"
            data-testid="vss-helper-toggle"
            :checked="settings.settings.windows.vssHelper"
            @change="setVssHelper"
          />
          {{ t("settings.rules.vssHelperLabel") }}
        </span>
        <p class="text-xs text-zinc-500 dark:text-zinc-400">
          {{ t("settings.rules.vssHelperNote") }}
        </p>
      </label>

      <!-- Issue #25 (launch UX): attended-elevation launch feedback. -->
      <p
        v-if="showVssPending"
        data-testid="vss-helper-pending"
        class="text-xs text-teal-700 dark:text-teal-300"
      >
        {{ t("settings.rules.vssHelperPending") }}
      </p>
      <p
        v-else-if="showVssDeclined"
        data-testid="vss-helper-declined"
        class="text-xs text-amber-700 dark:text-amber-300"
      >
        {{ t("settings.rules.vssHelperDeclined") }}
      </p>

      <!-- macOS twin of the VSS helper above (DESIGN s5.3.2). `macos` is
           non-null only on macOS, so exactly one of the two blocks renders. -->
      <label
        v-if="settings.settings.macos"
        class="block space-y-1"
        data-testid="apfs-snapshot-setting"
      >
        <span class="flex items-center gap-2">
          <input
            type="checkbox"
            class="accent-teal-600"
            data-testid="apfs-snapshot-toggle"
            :checked="settings.settings.macos.apfsSnapshot"
            @change="setApfsSnapshot"
          />
          {{ t("settings.rules.apfsSnapshotLabel") }}
        </span>
        <p class="text-xs text-zinc-500 dark:text-zinc-400">
          {{ t("settings.rules.apfsSnapshotNote") }}
        </p>
        <p data-testid="apfs-snapshot-tcc-note" class="text-xs text-zinc-500 dark:text-zinc-400">
          {{ t("settings.rules.apfsSnapshotTccNote") }}
        </p>
      </label>

      <!-- Attended-administrator launch feedback (DESIGN s5.3.2). -->
      <p
        v-if="showApfsPending"
        data-testid="apfs-helper-pending"
        class="text-xs text-teal-700 dark:text-teal-300"
      >
        {{ t("settings.rules.apfsHelperPending") }}
      </p>
      <p
        v-else-if="showApfsDeclined"
        data-testid="apfs-helper-declined"
        class="text-xs text-amber-700 dark:text-amber-300"
      >
        {{ t("settings.rules.apfsHelperDeclined") }}
      </p>
    </section>

    <!-- Menu bar extra (macOS only, spec 2026-07-31 s2). -->
    <section
      v-if="settings.settings.macos"
      class="space-y-2"
      :class="cardCls"
      data-testid="menubar-setting"
    >
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.sections.menuBar") }}
      </h3>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("settings.rules.menuBarIntro") }}
      </p>

      <div
        data-testid="menubar-preview"
        class="w-fit rounded-full bg-zinc-900 px-3 py-1 text-xs text-white dark:bg-black"
      >
        {{ sampleTitle }}
      </div>

      <div class="space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("settings.rules.menuBarShowLabel")
        }}</span>
        <label class="flex items-center gap-2">
          <input
            type="checkbox"
            class="accent-teal-600"
            data-testid="menubar-speed-toggle"
            :checked="settings.settings.macos.menuBar?.showUploadSpeed ?? false"
            @change="setMenuBarSpeed"
          />
          {{ t("settings.rules.menuBarSpeed") }}
        </label>
        <label class="flex items-center gap-2">
          <input
            type="checkbox"
            class="accent-teal-600"
            data-testid="menubar-percent-toggle"
            :checked="settings.settings.macos.menuBar?.showPercent ?? false"
            @change="setMenuBarPercent"
          />
          {{ t("settings.rules.menuBarPercent") }}
        </label>
        <label class="flex items-center gap-2">
          <input
            type="checkbox"
            class="accent-teal-600"
            data-testid="menubar-files-toggle"
            :checked="settings.settings.macos.menuBar?.showFiles ?? false"
            @change="setMenuBarFiles"
          />
          {{ t("settings.rules.menuBarFiles") }}
        </label>
        <label class="flex items-center gap-2">
          <input
            type="checkbox"
            class="accent-teal-600"
            data-testid="menubar-eta-toggle"
            :checked="settings.settings.macos.menuBar?.showEta ?? false"
            @change="setMenuBarEta"
          />
          {{ t("settings.rules.menuBarEta") }}
        </label>
      </div>

      <p v-if="menuBarEnabledCount >= 3" class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("settings.rules.menuBarWidthHint") }}
      </p>

      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("settings.rules.menuBarIdleLabel")
        }}</span>
        <select
          data-testid="menubar-idle-select"
          class="w-full"
          :class="inputCls"
          :value="settings.settings.macos.menuBar?.idle ?? 'none'"
          @change="setMenuBarIdle"
        >
          <option v-for="idle in menuBarIdleOptions" :key="idle" :value="idle">
            {{ t(menuBarIdleLabelKey(idle)) }}
          </option>
        </select>
      </label>
    </section>
  </div>
</template>
