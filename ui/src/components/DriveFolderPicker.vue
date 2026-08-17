<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import type { DriveFolderEntry } from "../ipc/types";

// Shared destination picker (SPEC s11.2; DESIGN s8.5 step 3). Used by BOTH
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
//
// Issue #306 (sort/filter/near-fullscreen) and issue #307 (create/rename) add:
// a client-side sort + substring filter over the CURRENT folder's already
// fully-fetched listing (never recursive - every page of even a huge folder is
// fetched by `pickDriveFolder` up front, so filtering never feels incomplete);
// an inline "New folder" row (every backend); and an inline per-row rename,
// offered only when `supportsRename` says the backend has a rename primitive
// (`BackendKind::supports_rename` - S3 does not, its "folders" are key
// prefixes with no separate identity to rename). Both actions surface errors
// INLINE, next to the control that failed - never an alert dialog.

const { t, te, locale } = useI18n();

const props = defineProps<{
  accountId: string | null;
  backendKind?: string;
  /** Issue #307: whether this backend's folders can be renamed in place
   * (`BackendDto.supportsRename`). When false the rename pencil is replaced
   * by a disabled control explaining why, rather than omitted - so the
   * affordance's ABSENCE reads as a property of the destination. */
  supportsRename?: boolean;
  /** Issue #306: near-fullscreen sizing for a context that gives the picker
   * the whole viewport (the add-source wizard's destination-folder step) -
   * the list FLEXES to fill its container instead of capping at a fixed
   * height. Defaults to false, which keeps a generous but bounded height. */
  fill?: boolean;
}>();
const emit = defineEmits<{ (e: "error", err: unknown): void }>();

/** What the destination ROOT is called at this backend. The picker is shared by
 * every browsable destination, so a hard-coded "My Drive" was simply wrong for an
 * S3 account looking at its bucket root. Falls back to a neutral label for a
 * backend with no seeded name, so a destination added on the Rust side never
 * renders as a Drive-ism or as a blank crumb. */
const rootName = computed(() => {
  const key = `drivePicker.root.${props.backendKind}`;
  return props.backendKind !== undefined && te(key) ? t(key) : t("drivePicker.rootName");
});

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

// --- Issue #306: client-side sort + filter --------------------------------

type SortKey = "nameAsc" | "nameDesc" | "modifiedAsc" | "modifiedDesc";
/** Remembered for the picker's session (this component instance), not
 *  persisted across app restarts - the mockup's stated default. */
const sortKey = ref<SortKey>("nameAsc");
const filterQuery = ref("");
const filterInput = ref<HTMLInputElement | null>(null);

/** Sort ORDINARY folders only - a Shared Drive root (issue #7) is a distinct
 *  category the backend always puts first, and reordering it in with named
 *  folders would bury the drives a My Drive root exists to surface. */
function sortOrdinary(list: DriveFolderEntry[], key: SortKey): DriveFolderEntry[] {
  const copy = [...list];
  if (key === "nameAsc") {
    copy.sort((a, b) => a.name.localeCompare(b.name));
  } else if (key === "nameDesc") {
    copy.sort((a, b) => b.name.localeCompare(a.name));
  } else {
    // Modified sort: a folder with no timestamp (S3's key-prefix "folders"
    // carry none) sorts LAST regardless of direction, never fabricating a
    // date to justify a position.
    const dir = key === "modifiedAsc" ? 1 : -1;
    copy.sort((a, b) => {
      const am = a.modifiedTime ?? null;
      const bm = b.modifiedTime ?? null;
      if (am === null && bm === null) return a.name.localeCompare(b.name);
      if (am === null) return 1;
      if (bm === null) return -1;
      return dir * (am - bm);
    });
  }
  return copy;
}

/** Folders currently shown, per the sort + filter controls. Client-side, over
 *  the CURRENT folder's already fully-fetched listing only. */
const displayedFolders = computed(() => {
  const q = filterQuery.value.trim().toLowerCase();
  const matched = q ? folders.value.filter((f) => f.name.toLowerCase().includes(q)) : folders.value;
  const shared = matched.filter((f) => f.isSharedDrive);
  const ordinary = matched.filter((f) => !f.isSharedDrive);
  return [...shared, ...sortOrdinary(ordinary, sortKey.value)];
});

const dateFormatter = computed(
  () => new Intl.DateTimeFormat(locale.value, { dateStyle: "medium" })
);
function formatModified(epochMs: number): string {
  return dateFormatter.value.format(new Date(epochMs));
}

// --- Issue #307: create + rename ------------------------------------------

const creatingFolder = ref(false);
const newFolderName = ref("");
const createBusy = ref(false);
const createErrorCode = ref<string | null>(null);

function startCreateFolder(): void {
  creatingFolder.value = true;
  newFolderName.value = "";
  createErrorCode.value = null;
}

function cancelCreateFolder(): void {
  creatingFolder.value = false;
  newFolderName.value = "";
  createErrorCode.value = null;
}

async function confirmCreateFolder(): Promise<void> {
  const name = newFolderName.value.trim();
  if (name === "" || props.accountId === null || folderId.value === null || createBusy.value) {
    return;
  }
  createBusy.value = true;
  createErrorCode.value = null;
  try {
    const entry = await ipc.createRemoteFolder(
      props.accountId,
      folderId.value,
      name,
      driveId.value
    );
    // ensure_folder ADOPTS an existing same-name match rather than erroring
    // (SPEC s3), so a folder created twice in a row must not appear twice.
    folders.value = [...folders.value.filter((f) => f.id !== entry.id), entry];
    creatingFolder.value = false;
    newFolderName.value = "";
  } catch (e) {
    createErrorCode.value = toErrorCode(e);
  } finally {
    createBusy.value = false;
  }
}

const renamingFolderId = ref<string | null>(null);
const renameText = ref("");
const renameBusy = ref(false);
const renameErrorCode = ref<string | null>(null);

function startRename(folder: DriveFolderEntry): void {
  if (!props.supportsRename) return;
  renamingFolderId.value = folder.id;
  renameText.value = folder.name;
  renameErrorCode.value = null;
}

function cancelRename(): void {
  renamingFolderId.value = null;
  renameText.value = "";
  renameErrorCode.value = null;
}

async function confirmRename(): Promise<void> {
  const id = renamingFolderId.value;
  const name = renameText.value.trim();
  if (id === null || name === "" || props.accountId === null || renameBusy.value) return;
  renameBusy.value = true;
  renameErrorCode.value = null;
  try {
    const updated = await ipc.renameRemoteFolder(props.accountId, id, name, driveId.value);
    // An SFTP rename can mint a NEW id (an SFTP id is path-derived, unlike
    // Drive's opaque one) - replace the whole row by its OLD id rather than
    // patch the name in place, so a later click uses the fresh id the
    // backend just minted, never the one that stopped resolving.
    folders.value = folders.value.map((f) => (f.id === id ? updated : f));
    renamingFolderId.value = null;
    renameText.value = "";
  } catch (e) {
    renameErrorCode.value = toErrorCode(e);
  } finally {
    renameBusy.value = false;
  }
}

async function loadFolder(crumb: Crumb): Promise<void> {
  if (props.accountId === null) return;
  // A folder-crossing navigation abandons any in-progress row action and the
  // typed filter - both belong to the listing being left.
  cancelCreateFolder();
  cancelRename();
  filterQuery.value = "";
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
  // The filter focuses automatically when the picker opens (once per open,
  // not on every later navigation - that would steal focus while browsing).
  await nextTick();
  filterInput.value?.focus();
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
  <div
    class="space-y-3"
    :class="fill ? 'flex min-h-0 flex-1 flex-col' : ''"
    data-testid="drive-folder-picker"
  >
    <div v-if="accountId" class="flex flex-wrap items-center gap-2">
      <nav class="flex flex-1 flex-wrap items-center gap-1 text-xs">
        <template v-for="(crumb, i) in crumbs" :key="i">
          <span v-if="i > 0" class="text-zinc-400 dark:text-zinc-600" aria-hidden="true">/</span>
          <button
            type="button"
            class="rounded-sm px-1 py-0.5 text-zinc-600 transition-colors hover:text-teal-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-teal-500 dark:text-zinc-400 dark:hover:text-teal-300"
            @click="goToCrumb(i)"
          >
            {{ i === 0 ? rootName : crumb.path.split("/").pop() }}
          </button>
        </template>
      </nav>

      <!-- Issue #306: type-to-filter, current folder only, focused on open. -->
      <div class="relative">
        <svg
          class="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-zinc-400 dark:text-zinc-500"
          viewBox="0 0 16 16"
          fill="none"
          stroke="currentColor"
          stroke-width="1.6"
          aria-hidden="true"
        >
          <circle cx="6.8" cy="6.8" r="4.3" />
          <line x1="10" y1="10" x2="13.5" y2="13.5" />
        </svg>
        <input
          ref="filterInput"
          v-model="filterQuery"
          type="text"
          class="w-40 rounded-md border border-zinc-300 bg-white py-1.5 pr-2 pl-7 text-xs text-zinc-900 placeholder:text-zinc-400 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 sm:w-48 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100"
          :placeholder="t('drivePicker.filterPlaceholder')"
          :aria-label="t('drivePicker.filterPlaceholder')"
          data-testid="drive-picker-filter"
        />
      </div>

      <!-- Issue #306: sort control - Name asc (default) / Name desc /
           Modified asc / Modified desc. -->
      <select
        v-model="sortKey"
        class="rounded-md border border-zinc-300 bg-white py-1.5 px-2 text-xs text-zinc-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200"
        :aria-label="t('drivePicker.sortLabel')"
        data-testid="drive-picker-sort"
      >
        <option value="nameAsc">{{ t("drivePicker.sort.nameAsc") }}</option>
        <option value="nameDesc">{{ t("drivePicker.sort.nameDesc") }}</option>
        <option value="modifiedAsc">{{ t("drivePicker.sort.modifiedAsc") }}</option>
        <option value="modifiedDesc">{{ t("drivePicker.sort.modifiedDesc") }}</option>
      </select>

      <!-- Issue #307: create a folder, on every backend. -->
      <button
        type="button"
        class="inline-flex shrink-0 items-center gap-1 rounded-md bg-teal-700 px-2.5 py-1.5 text-xs font-medium text-white transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50"
        :disabled="creatingFolder || folderId === null"
        data-testid="drive-picker-new-folder"
        @click="startCreateFolder"
      >
        <svg
          class="size-3.5"
          viewBox="0 0 16 16"
          fill="none"
          stroke="currentColor"
          stroke-width="1.8"
          aria-hidden="true"
        >
          <line x1="8" y1="3" x2="8" y2="13" />
          <line x1="3" y1="8" x2="13" y2="8" />
        </svg>
        {{ t("drivePicker.newFolderButton") }}
      </button>
    </div>

    <p v-if="loading" class="text-sm text-zinc-500">
      {{ t("common.loading") }}
    </p>
    <template v-else-if="accountId">
      <ul
        v-if="displayedFolders.length > 0 || creatingFolder"
        class="divide-y divide-zinc-200 overflow-auto rounded-md border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-700"
        :class="fill ? 'min-h-0 flex-1' : 'max-h-96'"
      >
        <li
          v-if="creatingFolder"
          class="flex items-center gap-2 px-3 py-2"
          data-testid="drive-picker-create-row"
        >
          <input
            v-model="newFolderName"
            type="text"
            class="min-w-0 flex-1 rounded-sm border border-teal-500 bg-white px-2 py-1 text-sm text-zinc-900 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 dark:bg-zinc-900 dark:text-zinc-100"
            :placeholder="t('drivePicker.newFolderPlaceholder')"
            :disabled="createBusy"
            data-testid="drive-picker-create-input"
            @keydown.enter="confirmCreateFolder"
            @keydown.escape="cancelCreateFolder"
          />
          <button
            type="button"
            class="flex size-6 shrink-0 items-center justify-center rounded-sm border border-teal-600 text-teal-700 transition-colors hover:bg-teal-50 disabled:opacity-50 dark:text-teal-300 dark:hover:bg-teal-950/40"
            :disabled="createBusy || newFolderName.trim() === ''"
            :aria-label="t('drivePicker.confirmCreate')"
            data-testid="drive-picker-create-confirm"
            @click="confirmCreateFolder"
          >
            <svg
              class="size-3.5"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              aria-hidden="true"
            >
              <polyline points="3,8.5 6.5,12 13,4" />
            </svg>
          </button>
          <button
            type="button"
            class="flex size-6 shrink-0 items-center justify-center rounded-sm border border-zinc-300 text-zinc-500 transition-colors hover:bg-zinc-100 disabled:opacity-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
            :disabled="createBusy"
            :aria-label="t('drivePicker.cancelCreate')"
            data-testid="drive-picker-create-cancel"
            @click="cancelCreateFolder"
          >
            <svg
              class="size-3.5"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              aria-hidden="true"
            >
              <line x1="4" y1="4" x2="12" y2="12" />
              <line x1="12" y1="4" x2="4" y2="12" />
            </svg>
          </button>
        </li>
        <li
          v-if="creatingFolder && createErrorCode"
          class="px-3 pb-2 text-xs text-red-600"
          role="alert"
          data-testid="drive-picker-create-error"
        >
          {{ t(`errors.${createErrorCode}.short`) }}
        </li>

        <li
          v-for="folder in displayedFolders"
          :key="folder.id"
          class="group relative flex items-center"
        >
          <template v-if="renamingFolderId === folder.id">
            <input
              v-model="renameText"
              type="text"
              class="mx-3 my-1.5 min-w-0 flex-1 rounded-sm border border-teal-500 bg-white px-2 py-1 text-sm text-zinc-900 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 dark:bg-zinc-900 dark:text-zinc-100"
              :disabled="renameBusy"
              :data-testid="`drive-picker-rename-input-${folder.id}`"
              @keydown.enter="confirmRename"
              @keydown.escape="cancelRename"
            />
            <button
              type="button"
              class="mr-1 flex size-6 shrink-0 items-center justify-center rounded-sm border border-teal-600 text-teal-700 transition-colors hover:bg-teal-50 disabled:opacity-50 dark:text-teal-300 dark:hover:bg-teal-950/40"
              :disabled="renameBusy || renameText.trim() === ''"
              :aria-label="t('drivePicker.confirmRename')"
              :data-testid="`drive-picker-rename-confirm-${folder.id}`"
              @click="confirmRename"
            >
              <svg
                class="size-3.5"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                aria-hidden="true"
              >
                <polyline points="3,8.5 6.5,12 13,4" />
              </svg>
            </button>
            <button
              type="button"
              class="mr-2 flex size-6 shrink-0 items-center justify-center rounded-sm border border-zinc-300 text-zinc-500 transition-colors hover:bg-zinc-100 disabled:opacity-50 dark:border-zinc-700 dark:text-zinc-400 dark:hover:bg-zinc-800"
              :disabled="renameBusy"
              :aria-label="t('drivePicker.cancelRename')"
              :data-testid="`drive-picker-rename-cancel-${folder.id}`"
              @click="cancelRename"
            >
              <svg
                class="size-3.5"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                aria-hidden="true"
              >
                <line x1="4" y1="4" x2="12" y2="12" />
                <line x1="12" y1="4" x2="4" y2="12" />
              </svg>
            </button>
          </template>
          <template v-else>
            <button
              type="button"
              class="flex flex-1 items-center gap-2 px-3 py-2 text-left text-sm transition-colors hover:bg-teal-50 focus-visible:outline-solid focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-teal-500 dark:hover:bg-zinc-800"
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
            <!-- Issue #306: last-modified, where the backend provides one. -->
            <span
              v-if="folder.modifiedTime"
              class="shrink-0 px-2 font-mono text-[0.7rem] tabular-nums text-zinc-400 dark:text-zinc-500"
            >
              {{ formatModified(folder.modifiedTime) }}
            </span>
            <!-- Issue #307: inline rename, on hover. Drive + SFTP only - S3
                 gets a disabled control with an explanatory tooltip instead
                 of no control at all, so the absence reads as a property of
                 the destination rather than a bug. -->
            <button
              v-if="!folder.isSharedDrive && supportsRename"
              type="button"
              class="mr-2 shrink-0 rounded-sm p-1 text-zinc-400 opacity-0 transition-opacity group-hover:opacity-100 hover:text-teal-700 focus-visible:opacity-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 dark:hover:text-teal-300"
              :aria-label="t('drivePicker.renameAction', { name: folder.name })"
              :data-testid="`drive-picker-rename-${folder.id}`"
              @click.stop="startRename(folder)"
            >
              <svg
                class="size-3.5"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                aria-hidden="true"
              >
                <path d="M3.5 12.5 4 10l6.5-6.5 1.5 1.5L5.5 11.5z" />
                <line x1="1.5" y1="14" x2="4" y2="14" />
              </svg>
            </button>
            <span
              v-else-if="!folder.isSharedDrive"
              class="mr-2 shrink-0 rounded-sm p-1 text-zinc-300 opacity-0 transition-opacity group-hover:opacity-100 dark:text-zinc-700"
              :title="t('drivePicker.renameUnsupportedTooltip')"
              data-testid="drive-picker-rename-disabled"
            >
              <svg
                class="size-3.5"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                aria-hidden="true"
              >
                <path d="M3.5 12.5 4 10l6.5-6.5 1.5 1.5L5.5 11.5z" />
                <line x1="1.5" y1="14" x2="4" y2="14" />
              </svg>
            </span>
          </template>
        </li>
        <li
          v-if="renamingFolderId && renameErrorCode"
          class="px-3 pb-2 text-xs text-red-600"
          role="alert"
          data-testid="drive-picker-rename-error"
        >
          {{ t(`errors.${renameErrorCode}.short`) }}
        </li>
      </ul>
      <p
        v-else
        class="rounded-md border border-dashed border-zinc-300 px-3 py-2 text-sm text-zinc-500 dark:border-zinc-700"
      >
        {{ filterQuery.trim() !== "" ? t("drivePicker.noMatches") : t("drivePicker.empty") }}
      </p>
    </template>

    <p class="text-sm text-zinc-700 dark:text-zinc-200" data-testid="drive-destination">
      {{ t("drivePicker.destinationLabel") }}: {{ folderPath || rootName }}
    </p>
  </div>
</template>
