import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import {
  onUpdaterAvailable,
  onUpdaterDownloadProgress,
  onUpdaterDownloaded,
  type UpdaterDownloadProgressPayload,
} from "../ipc/events";
import type { ReleaseDto, UpdateInfo } from "../ipc/types";

/** Releases page size (matches the backend `list_releases` RELEASES_PER_PAGE). A
 * page shorter than this means there are no more pages. */
export const RELEASES_PER_PAGE = 10;

/**
 * In-app updater store (SPEC s15.2; ROADMAP M9). Drives the Settings > About
 * updater surface:
 * - the active channel (stable / dev) toggle, persisted via set_update_channel,
 * - a manual "check for updates" (check_for_update) that records the available
 *   update + shows the banner,
 * - the live `updater:available` banner (so a periodic background check surfaces
 *   without a manual click), with Install (install_update) + download progress
 *   (`updater:download_progress`) + `updater:downloaded`,
 * - the paginated GitHub releases list (list_releases) for the release-notes
 *   viewer, with a ChangelogModal per entry.
 *
 * Command errors are normalized to the stable SPEC s24 `{ code }` shape and
 * exposed as `*ErrorCode` for the view to localize via `t(\`errors.${code}.long\`)`.
 */
export const useUpdaterStore = defineStore("updater", () => {
  // The active channel (stable | dev). Loaded via get_update_channel.
  const channel = ref<string>("stable");

  // The available update (from a manual check OR the live updater:available
  // event); null when up to date / not yet checked.
  const available = ref<UpdateInfo | null>(null);
  // True after a manual check has run at least once (so the UI can show an
  // explicit "you are up to date" vs "not checked yet").
  const checked = ref(false);
  const checking = ref(false);
  const checkErrorCode = ref<string | null>(null);

  // Install + download progress.
  const installing = ref(false);
  const installErrorCode = ref<string | null>(null);
  const downloaded = ref(0);
  const downloadTotal = ref<number | null>(null);
  // True once the staged update finished downloading (updater:downloaded), just
  // before the app relaunches.
  const downloadComplete = ref(false);

  // The in-app "update available" banner visibility (dismissable).
  const bannerDismissed = ref(false);

  // The paginated releases list (release-notes viewer).
  const releases = ref<ReleaseDto[]>([]);
  const releasesPage = ref(0);
  const releasesLoading = ref(false);
  const releasesErrorCode = ref<string | null>(null);
  const hasMoreReleases = ref(false);

  // The release whose changelog the modal is showing (null = closed).
  const changelogRelease = ref<ReleaseDto | null>(null);

  /** Whether the update-available banner should be shown. */
  const bannerVisible = computed(() => available.value !== null && !bannerDismissed.value);

  /** Download progress as a 0..1 fraction, or null when the total is unknown. */
  const downloadFraction = computed<number | null>(() => {
    if (downloadTotal.value === null || downloadTotal.value <= 0) return null;
    return Math.min(1, downloaded.value / downloadTotal.value);
  });

  /** Load the active channel from settings. */
  async function loadChannel(): Promise<void> {
    try {
      channel.value = await ipc.getUpdateChannel();
    } catch (e) {
      checkErrorCode.value = toErrorCode(e);
    }
  }

  /** Switch the active channel and persist it. Reloads the releases list since a
   * channel change can change which releases are eligible. */
  async function setChannel(next: string): Promise<void> {
    checkErrorCode.value = null;
    try {
      channel.value = await ipc.setUpdateChannel(next);
      // A channel change can change "latest"; clear a stale available + refresh.
      available.value = null;
      checked.value = false;
      await loadReleases();
    } catch (e) {
      checkErrorCode.value = toErrorCode(e);
    }
  }

  /** Run a manual update check. Records the available update (or clears it when up
   * to date) and surfaces the banner on an available update. */
  async function check(): Promise<void> {
    checking.value = true;
    checkErrorCode.value = null;
    try {
      const result = await ipc.checkForUpdate();
      available.value = result;
      checked.value = true;
      if (result !== null) {
        // A fresh available update re-shows the banner even if a prior one was
        // dismissed.
        bannerDismissed.value = false;
      }
    } catch (e) {
      checkErrorCode.value = toErrorCode(e);
    } finally {
      checking.value = false;
    }
  }

  /** Ingest a live `updater:available` event (from the periodic background
   * check): record the update + un-dismiss the banner so it surfaces. */
  function onAvailable(info: UpdateInfo): void {
    available.value = info;
    checked.value = true;
    bannerDismissed.value = false;
  }

  /** R2-P1-3: hydrate the banner from the backend's recorded PENDING update on
   * startup. The startup periodic check can find + record an update + emit
   * `updater:available` before the webview attaches its listeners, so that
   * one-shot event is lost; this fills the gap. No-op when nothing is pending or
   * when a live event already surfaced an update (so we never clobber fresher
   * state). Does NOT un-dismiss a banner the user already closed. */
  async function hydratePending(): Promise<void> {
    if (available.value !== null) return;
    try {
      const info = await ipc.getPendingUpdateInfo();
      if (info !== null && available.value === null) {
        available.value = info;
        checked.value = true;
      }
    } catch (e) {
      // Hydration is best-effort; surface as the check error channel but never
      // throw out of app boot.
      checkErrorCode.value = toErrorCode(e);
    }
  }

  /** Ingest a `updater:download_progress` event into the progress bar. */
  function onProgress(payload: UpdaterDownloadProgressPayload): void {
    downloaded.value = payload.downloaded;
    downloadTotal.value = payload.total;
  }

  /** Ingest a `updater:downloaded` event (staging finished; relaunch imminent). */
  function onDownloaded(): void {
    downloadComplete.value = true;
  }

  /** Download + apply the pending update and relaunch. On success the app
   * restarts (this never resolves); a failure surfaces the s24 code. */
  async function install(): Promise<void> {
    installing.value = true;
    installErrorCode.value = null;
    downloaded.value = 0;
    downloadTotal.value = null;
    downloadComplete.value = false;
    try {
      await ipc.installUpdate();
      // On success the backend relaunches; control rarely returns here.
    } catch (e) {
      installErrorCode.value = toErrorCode(e);
      installing.value = false;
    }
  }

  /** Dismiss the update-available banner (until the next available update). */
  function dismissBanner(): void {
    bannerDismissed.value = true;
  }

  /** Open the changelog modal for a specific release (or the available update,
   * mapped to a release-shaped object). */
  function openChangelog(release: ReleaseDto): void {
    changelogRelease.value = release;
  }

  /** Open the changelog modal for the currently-available update. */
  function openAvailableChangelog(): void {
    if (available.value === null) return;
    changelogRelease.value = {
      version: available.value.version,
      name: available.value.version,
      notes: available.value.notes ?? "",
      publishedAt: available.value.publishedAt ?? "",
      url: "",
    };
  }

  /** Close the changelog modal. */
  function closeChangelog(): void {
    changelogRelease.value = null;
  }

  /** Load the first page of releases (resets pagination). */
  async function loadReleases(): Promise<void> {
    releasesLoading.value = true;
    releasesErrorCode.value = null;
    releasesPage.value = 1;
    try {
      const page = await ipc.listReleases(1);
      releases.value = page;
      hasMoreReleases.value = page.length === RELEASES_PER_PAGE;
    } catch (e) {
      releasesErrorCode.value = toErrorCode(e);
      releases.value = [];
      hasMoreReleases.value = false;
    } finally {
      releasesLoading.value = false;
    }
  }

  /** Load the next page of releases and append it. No-op when none remain. */
  async function loadMoreReleases(): Promise<void> {
    if (!hasMoreReleases.value || releasesLoading.value) return;
    releasesLoading.value = true;
    releasesErrorCode.value = null;
    const nextPage = releasesPage.value + 1;
    try {
      const page = await ipc.listReleases(nextPage);
      releases.value = [...releases.value, ...page];
      releasesPage.value = nextPage;
      hasMoreReleases.value = page.length === RELEASES_PER_PAGE;
    } catch (e) {
      releasesErrorCode.value = toErrorCode(e);
    } finally {
      releasesLoading.value = false;
    }
  }

  // --- event subscriptions (no listener leak) -------------------------------
  let unlistenAvailable: UnlistenFn | null = null;
  let unlistenProgress: UnlistenFn | null = null;
  let unlistenDownloaded: UnlistenFn | null = null;
  let desiredSubscribed = false;

  /** Subscribe to the updater event stream (idempotent).
   *
   * R4-P2-1: register every listener with cleanup-on-partial-failure. The prior
   * `Promise.all` form leaked handles when ONE registration rejected: the other
   * (already-resolved) listeners stayed attached but their unlisten handles were
   * dropped on the floor (never assigned), AND `desiredSubscribed` was left
   * `true`, so a later retry no-opped forever - the store could never re-subscribe
   * and an `updater:available` event could fire into a dead store. Now we collect
   * whatever resolved (`allSettled`), and on ANY rejection we unlisten everything
   * that DID register, reset `desiredSubscribed=false` (so a retry can try again),
   * and re-throw so the caller can log/retry. Only on full success do we keep the
   * handles. */
  async function subscribe(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;

    const results = await Promise.allSettled([
      onUpdaterAvailable((info) => onAvailable(info)),
      onUpdaterDownloadProgress((payload) => onProgress(payload)),
      onUpdaterDownloaded(() => onDownloaded()),
    ]);

    // Gather the handles that DID register (so we can tear them down on any
    // partial failure) and the first rejection reason (if any).
    const registered: UnlistenFn[] = [];
    let failure: unknown = null;
    for (const r of results) {
      if (r.status === "fulfilled") registered.push(r.value);
      else if (failure === null) failure = r.reason;
    }

    // If we were torn down while awaiting (unsubscribe() raced ahead), OR any
    // registration failed, unlisten everything that registered and reset state so
    // a future subscribe() can retry cleanly.
    if (!desiredSubscribed || failure !== null) {
      for (const un of registered) {
        try {
          un();
        } catch {
          // Best-effort teardown; never mask the original failure.
        }
      }
      unlistenAvailable = null;
      unlistenProgress = null;
      unlistenDownloaded = null;
      desiredSubscribed = false;
      if (failure !== null) {
        // Propagate so the caller (App.vue boot) can log + a later retry can
        // re-subscribe. Hydration still runs (App.vue guards it).
        throw failure;
      }
      return;
    }

    // Full success: keep the handles. `results` is in the same order as the
    // listener array above, so each fulfilled value maps to its event.
    const [a, p, d] = registered;
    unlistenAvailable = a;
    unlistenProgress = p;
    unlistenDownloaded = d;
  }

  /** Stop the updater event subscriptions. */
  function unsubscribe(): void {
    desiredSubscribed = false;
    if (unlistenAvailable) {
      unlistenAvailable();
      unlistenAvailable = null;
    }
    if (unlistenProgress) {
      unlistenProgress();
      unlistenProgress = null;
    }
    if (unlistenDownloaded) {
      unlistenDownloaded();
      unlistenDownloaded = null;
    }
  }

  return {
    channel,
    available,
    checked,
    checking,
    checkErrorCode,
    installing,
    installErrorCode,
    downloaded,
    downloadTotal,
    downloadComplete,
    bannerDismissed,
    releases,
    releasesPage,
    releasesLoading,
    releasesErrorCode,
    hasMoreReleases,
    changelogRelease,
    bannerVisible,
    downloadFraction,
    loadChannel,
    setChannel,
    check,
    onAvailable,
    onProgress,
    onDownloaded,
    hydratePending,
    install,
    dismissBanner,
    openChangelog,
    openAvailableChangelog,
    closeChangelog,
    loadReleases,
    loadMoreReleases,
    subscribe,
    unsubscribe,
  };
});
