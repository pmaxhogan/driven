import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Updater store tests (SPEC s15.2; ROADMAP M9). The seams are
// `@tauri-apps/api/core`'s `invoke` (every typed IPC wrapper routes through it)
// and `@tauri-apps/api/event`'s `listen` (the updater event subscriptions).
// Mocking both lets us drive: channel get/set round-trip, a manual check
// (available vs up-to-date) -> banner state, the install + download-progress
// flow, a live `updater:available` event -> banner, and releases pagination -
// all against a fake backend with NO real updater / network.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

// Capture the updater event handlers so the test can fire events on demand.
const handlers: Record<string, (payload: unknown) => void> = {};
// A SINGLE spy every per-listener unlisten delegates to, so existing tests can
// still assert the total teardown count.
const unlistenMock = vi.fn();
// Per-event unlisten spies (so a leak test can assert WHICH listeners were torn
// down on a partial failure), keyed by event name.
const perEventUnlisten: Record<string, ReturnType<typeof vi.fn>> = {};
// Event names whose `listen()` should REJECT (simulate a registration failure).
const failEvents = new Set<string>();

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(
    async (event: string, cb: (e: { payload: unknown }) => void) => {
      if (failEvents.has(event)) {
        throw new Error(`listen failed for ${event}`);
      }
      handlers[event] = (payload: unknown) => cb({ payload });
      const un = vi.fn(() => {
        // Model a real unlisten: a torn-down listener no longer fires.
        delete handlers[event];
        unlistenMock();
      });
      perEventUnlisten[event] = un;
      return un;
    },
  ),
}));

import { useUpdaterStore, RELEASES_PER_PAGE } from "../stores/updater";
import type { ReleaseDto, UpdateInfo } from "../ipc/types";

function release(version: string): ReleaseDto {
  return {
    version,
    name: `Driven ${version}`,
    notes: `Notes for ${version}`,
    publishedAt: "2026-06-24T00:00:00Z",
    url: `https://github.com/pmaxhogan/driven/releases/${version}`,
  };
}

function update(version: string): UpdateInfo {
  return {
    version,
    notes: `Release notes ${version}`,
    publishedAt: "2026-06-24T00:00:00Z",
    channel: "stable",
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  for (const k of Object.keys(handlers)) delete handlers[k];
  for (const k of Object.keys(perEventUnlisten)) delete perEventUnlisten[k];
  failEvents.clear();
  unlistenMock.mockReset();
});

describe("updater store", () => {
  it("loads + round-trips the channel via get/set_update_channel", async () => {
    invokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      if (cmd === "set_update_channel") {
        return Promise.resolve((args as { channel: string }).channel);
      }
      if (cmd === "list_releases") return Promise.resolve([]);
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    await store.loadChannel();
    expect(store.channel).toBe("stable");

    await store.setChannel("dev");
    expect(store.channel).toBe("dev");
    // set persisted via set_update_channel with the new value.
    expect(invokeMock).toHaveBeenCalledWith("set_update_channel", {
      channel: "dev",
    });
  });

  it("check() surfaces an available update + shows the banner", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "check_for_update") return Promise.resolve(update("0.2.0"));
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    await store.check();

    expect(store.checked).toBe(true);
    expect(store.available?.version).toBe("0.2.0");
    // The banner is visible (available + not dismissed).
    expect(store.bannerVisible).toBe(true);
  });

  it("check() reports up-to-date when no update is available", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "check_for_update") return Promise.resolve(null);
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    await store.check();

    expect(store.checked).toBe(true);
    expect(store.available).toBeNull();
    expect(store.bannerVisible).toBe(false);
  });

  it("a live updater:available event surfaces the banner", async () => {
    invokeMock.mockResolvedValue(null);
    const store = useUpdaterStore();
    await store.subscribe();

    // Banner hidden until an update arrives.
    expect(store.bannerVisible).toBe(false);

    handlers["updater:available"]?.(update("0.3.0"));
    expect(store.available?.version).toBe("0.3.0");
    expect(store.bannerVisible).toBe(true);
  });

  it("R2-P1-3: a live updater:available sets the banner even with NO view mounted (root subscription)", async () => {
    // The app-root (App.vue) owns the subscription, so an event that fires while
    // About is not mounted still drives the shared store banner. We model the
    // root by subscribing directly on the store (no component) and firing the
    // event - the banner must surface purely from store state.
    invokeMock.mockResolvedValue(null);
    const store = useUpdaterStore();
    await store.subscribe();

    // Nothing mounted, no manual check - banner hidden.
    expect(store.bannerVisible).toBe(false);

    // The backend's startup periodic check emits while About is closed.
    handlers["updater:available"]?.(update("0.4.0"));

    expect(store.available?.version).toBe("0.4.0");
    expect(store.bannerVisible).toBe(true);
    expect(store.checked).toBe(true);
  });

  it("R2-P1-3: hydratePending() fills the banner from the backend pending update on startup", async () => {
    // A startup emit can be lost if it fires before the webview attaches; the
    // app-root boot hydrates from get_pending_update_info so the banner still
    // appears.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_pending_update_info") return Promise.resolve(update("0.5.0"));
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    expect(store.available).toBeNull();

    await store.hydratePending();

    expect(store.available?.version).toBe("0.5.0");
    expect(store.bannerVisible).toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("get_pending_update_info", undefined);
  });

  it("R2-P1-3: hydratePending() does NOT clobber an already-available update", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_pending_update_info") return Promise.resolve(update("0.1.0"));
      return Promise.resolve(null);
    });
    const store = useUpdaterStore();
    await store.subscribe();
    // A fresher live event arrived first.
    handlers["updater:available"]?.(update("0.9.0"));
    expect(store.available?.version).toBe("0.9.0");

    // Hydration must not overwrite the fresher live value.
    await store.hydratePending();
    expect(store.available?.version).toBe("0.9.0");
    // get_pending_update_info is short-circuited (available already set).
    expect(invokeMock).not.toHaveBeenCalledWith(
      "get_pending_update_info",
      undefined,
    );
  });

  it("dismissBanner hides the banner without clearing the update", async () => {
    invokeMock.mockResolvedValue(null);
    const store = useUpdaterStore();
    await store.subscribe();
    handlers["updater:available"]?.(update("0.3.0"));
    expect(store.bannerVisible).toBe(true);

    store.dismissBanner();
    expect(store.bannerVisible).toBe(false);
    // The update info is still tracked.
    expect(store.available?.version).toBe("0.3.0");
  });

  it("install() streams download progress to a 0..1 fraction", async () => {
    // install_update never resolves on success (the app relaunches); model a
    // pending promise so `installing` stays true while progress streams.
    let resolveInstall: () => void = () => {};
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "install_update") {
        return new Promise<void>((res) => {
          resolveInstall = res;
        });
      }
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    await store.subscribe();
    handlers["updater:available"]?.(update("0.2.0"));

    // Start the install (do not await - it stays pending).
    const installPromise = store.install();
    expect(store.installing).toBe(true);

    // Progress events update the fraction.
    handlers["updater:download_progress"]?.({ downloaded: 50, total: 200 });
    expect(store.downloaded).toBe(50);
    expect(store.downloadTotal).toBe(200);
    expect(store.downloadFraction).toBeCloseTo(0.25);

    handlers["updater:download_progress"]?.({ downloaded: 200, total: 200 });
    expect(store.downloadFraction).toBe(1);

    // The downloaded event flips downloadComplete.
    handlers["updater:downloaded"]?.(update("0.2.0"));
    expect(store.downloadComplete).toBe(true);

    resolveInstall();
    await installPromise;
  });

  it("install() surfaces a signature-failure error code", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "install_update") {
        return Promise.reject({ code: "update.signature_invalid", message: "bad sig" });
      }
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    await store.install();
    expect(store.installErrorCode).toBe("update.signature_invalid");
    expect(store.installing).toBe(false);
  });

  it("paginates releases: load first page then append more", async () => {
    const firstPage = Array.from({ length: RELEASES_PER_PAGE }, (_, i) =>
      release(`0.${i + 10}.0`),
    );
    const secondPage = [release("0.5.0"), release("0.4.0")];
    invokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "list_releases") {
        const page = (args as { page: number }).page;
        return Promise.resolve(page === 1 ? firstPage : secondPage);
      }
      return Promise.resolve(null);
    });

    const store = useUpdaterStore();
    await store.loadReleases();
    // A full page implies there may be more.
    expect(store.releases.length).toBe(RELEASES_PER_PAGE);
    expect(store.hasMoreReleases).toBe(true);
    expect(store.releasesPage).toBe(1);

    await store.loadMoreReleases();
    // The second (short) page is appended; no more pages remain.
    expect(store.releases.length).toBe(RELEASES_PER_PAGE + 2);
    expect(store.releasesPage).toBe(2);
    expect(store.hasMoreReleases).toBe(false);

    // Calling again is a no-op (no more pages).
    invokeMock.mockClear();
    await store.loadMoreReleases();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("openAvailableChangelog maps the available update into a release for the modal", async () => {
    invokeMock.mockResolvedValue(null);
    const store = useUpdaterStore();
    await store.subscribe();
    handlers["updater:available"]?.(update("0.2.0"));

    store.openAvailableChangelog();
    expect(store.changelogRelease?.version).toBe("0.2.0");
    expect(store.changelogRelease?.notes).toBe("Release notes 0.2.0");

    store.closeChangelog();
    expect(store.changelogRelease).toBeNull();
  });

  it("unsubscribe tears down every listener", async () => {
    invokeMock.mockResolvedValue(null);
    const store = useUpdaterStore();
    await store.subscribe();
    store.unsubscribe();
    // Three listeners (available, progress, downloaded) were torn down.
    expect(unlistenMock).toHaveBeenCalledTimes(3);
  });

  it("R4-P2-1: a partial subscribe failure leaks NO listener and allows a later successful subscribe + hydration", async () => {
    invokeMock.mockResolvedValue(null);
    const store = useUpdaterStore();

    // Make the LAST listener registration fail; the first two will have resolved.
    failEvents.add("updater:downloaded");
    await expect(store.subscribe()).rejects.toThrow();

    // The two listeners that DID register were torn down (no leak) - exactly the
    // count that successfully registered before the failure.
    expect(unlistenMock).toHaveBeenCalledTimes(2);
    expect(perEventUnlisten["updater:available"]).toHaveBeenCalledTimes(1);
    expect(perEventUnlisten["updater:download_progress"]).toHaveBeenCalledTimes(1);

    // A leaked listener would still fire into the store; assert it does NOT.
    handlers["updater:available"]?.(update("9.9.9"));
    expect(store.available).toBeNull();

    // A later subscribe must NOT no-op (desiredSubscribed was reset): clear the
    // forced failure and re-subscribe successfully.
    unlistenMock.mockClear();
    failEvents.clear();
    await store.subscribe();

    // Now a live event surfaces the banner (the retry's listeners are live).
    handlers["updater:available"]?.(update("1.0.0"));
    expect(store.available?.version).toBe("1.0.0");
    expect(store.bannerVisible).toBe(true);

    // And hydration still runs independently (App.vue calls it in a finally even
    // when subscribe rejected). Model the backend having recorded a pending
    // update and assert hydratePending fetches it. Use a fresh store (nothing
    // available yet) so hydratePending is not short-circuited by the live update.
    setActivePinia(createPinia());
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_pending_update_info") return Promise.resolve(update("2.0.0"));
      return Promise.resolve(null);
    });
    const freshStore = useUpdaterStore();
    expect(freshStore.available).toBeNull();
    await freshStore.hydratePending();
    expect(freshStore.available?.version).toBe("2.0.0");
  });
});
