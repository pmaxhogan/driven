<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";
import type { DriveFolderEntry } from "../ipc/types";

// Shared Drive destination picker (SPEC s11.2; DESIGN s8.5 step 3). Used by BOTH
// the first-run setup wizard AND the Settings "Add source" wizard, so the two
// flows can never drift again. They DID drift: the setup wizard had a degenerate
// single-shot button that silently targeted My Drive root, showed no confirmation
// (it bound feedback to the always-empty backend `currentFolderPath`), and gave
// no way to pick a subfolder - so it looked broken even though it "worked". This
// breadcrumb browser (previously only in AddSourceWizard) is now the single
// implementation both flows mount.
//
// Behavior: list a Drive folder's child folders, descend by clicking a folder,
// climb via the breadcrumb. The CURRENTLY-shown folder is the selected
// destination (published via the folderId + folderPath v-models). The Drive root
// ("My Drive") is itself a valid destination, so landing on the picker
// immediately selects it AND shows it - the feedback whose absence made the old
// button look dead.
//
// Breadcrumb path: the backend cannot derive the ancestor chain (it lists one
// folder's children, not the path TO it) and returns an EMPTY currentFolderPath,
// so this component maintains the human path itself in `crumbs` (parent/name) and
// publishes THAT as folderPath - keeping backup_sources.drive_folder_path real.
//
// Errors are emitted raw so each parent maps them in its own style: the setup
// wizard maps to a stable SPEC s24 code (errors.${code}.long); AddSourceWizard
// shows String(e). i18n: every visible string is a seeded key.

const { t } = useI18n();

const props = defineProps<{ accountId: string | null }>();
const emit = defineEmits<{ (e: "error", err: unknown): void }>();

const folderId = defineModel<string | null>("folderId", { default: null });
const folderPath = defineModel<string>("folderPath", { default: "" });
// Issue #7: the Google Shared Drive id the current destination lives in, or null
// for My Drive. Published so the parent persists it into AddSourceRequest.driveId.
const driveId = defineModel<string | null>("driveId", { default: null });

// Breadcrumb stack of the folders descended into; the first entry (null id) is
// My Drive root. "up" re-fetches an ancestor; descend appends a child. Each
// crumb carries the Shared Drive id it lives in (issue #7): null for My Drive,
// so a descent into a Shared Drive keeps the corpora=drive scope on the way
// down and back up the breadcrumb.
interface Crumb {
  id: string | null;
  path: string;
  driveId: string | null;
}
const crumbs = ref<Crumb[]>([]);
const folders = ref<DriveFolderEntry[]>([]);
const loading = ref(false);

async function loadFolder(crumb: Crumb): Promise<void> {
  if (props.accountId === null) return;
  loading.value = true;
  try {
    const listing = await ipc.pickDriveFolder(props.accountId, crumb.id, crumb.driveId);
    folders.value = listing.folders;
    // B1: the current folder is itself the selectable destination (the backend
    // echoes a concrete id - "root" for My Drive - never null).
    folderId.value = listing.currentFolderId;
    // Issue #7: publish the current drive context so the parent persists it.
    driveId.value = listing.driveId ?? null;
    // R4-P2-2: persist the client-maintained breadcrumb path (the backend
    // returns ""). Fall back to the backend value only at the root (empty crumb).
    folderPath.value = crumb.path || listing.currentFolderPath;
  } catch (e) {
    emit("error", e);
  } finally {
    loading.value = false;
  }
}

async function openRoot(): Promise<void> {
  crumbs.value = [{ id: null, path: "", driveId: null }];
  await loadFolder(crumbs.value[0]);
}

async function descendInto(folder: DriveFolderEntry): Promise<void> {
  const parentPath = folderPath.value;
  const crumb: Crumb = {
    id: folder.id,
    path: parentPath ? `${parentPath}/${folder.name}` : folder.name,
    // Descending a Shared Drive root switches the scope to that drive; an
    // ordinary folder inherits the drive it lives in (both carried on the
    // entry's driveId, which the backend stamps).
    driveId: folder.driveId ?? null,
  };
  crumbs.value.push(crumb);
  await loadFolder(crumb);
}

async function goToCrumb(index: number): Promise<void> {
  crumbs.value = crumbs.value.slice(0, index + 1);
  await loadFolder(crumbs.value[index]);
}

// Load My Drive root as soon as an account is available (and on mount). Landing
// on the picker selects the root, so the destination is never silently unset and
// the user always sees where they will back up.
watch(
  () => props.accountId,
  (id) => {
    if (id) void openRoot();
  },
  { immediate: true }
);
</script>

<template>
  <div class="space-y-3" data-testid="drive-folder-picker">
    <nav v-if="accountId" class="flex flex-wrap items-center gap-1 text-xs">
      <template v-for="(crumb, i) in crumbs" :key="i">
        <span v-if="i > 0" class="text-zinc-400 dark:text-zinc-600" aria-hidden="true">/</span>
        <button
          type="button"
          class="rounded-sm px-1 py-0.5 text-zinc-600 transition-colors hover:text-teal-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-teal-500 dark:text-zinc-400 dark:hover:text-teal-300"
          @click="goToCrumb(i)"
        >
          {{ i === 0 ? t("drivePicker.rootName") : crumb.path.split("/").pop() }}
        </button>
      </template>
    </nav>

    <p v-if="loading" class="text-sm text-zinc-500">
      {{ t("common.loading") }}
    </p>
    <template v-else-if="accountId">
      <ul
        v-if="folders.length > 0"
        class="max-h-56 divide-y divide-zinc-200 overflow-auto rounded-md border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-700"
      >
        <li v-for="folder in folders" :key="folder.id">
          <button
            type="button"
            class="flex w-full items-center gap-2 px-3 py-2 text-left text-sm transition-colors hover:bg-teal-50 focus-visible:outline-solid focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-teal-500 dark:hover:bg-zinc-800"
            @click="descendInto(folder)"
          >
            <span
              v-if="folder.isSharedDrive"
              class="rounded-sm bg-teal-100 px-1.5 py-0.5 text-[0.65rem] font-medium text-teal-800 dark:bg-teal-900 dark:text-teal-200"
            >
              {{ t("drivePicker.sharedDriveBadge") }}
            </span>
            {{ folder.name }}
          </button>
        </li>
      </ul>
      <p
        v-else
        class="rounded-md border border-dashed border-zinc-300 px-3 py-2 text-sm text-zinc-500 dark:border-zinc-700"
      >
        {{ t("drivePicker.empty") }}
      </p>
    </template>

    <p class="text-sm text-zinc-700 dark:text-zinc-200" data-testid="drive-destination">
      {{ t("drivePicker.destinationLabel") }}: {{ folderPath || t("drivePicker.rootName") }}
    </p>
  </div>
</template>
