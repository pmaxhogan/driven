// A scripted, declarative stand-in for the Tauri IPC layer.
//
// The whole app talks to the backend through exactly one hole in the wall:
// `window.__TAURI_INTERNALS__.invoke`. `@tauri-apps/api`'s `invoke` forwards
// straight to it, `listen` is itself an `invoke("plugin:event|listen", ...)`,
// and the plugin wrappers the app uses (`openUrl`, `getVersion`) are invokes
// too. Filling that hole with a scripted object therefore runs the REAL app -
// real router, real stores, real components - against a backend that answers
// from a table, with no Rust, no native webview and no Google account.
//
// Two consumers, one module:
//
//   - vitest (jsdom): `installMockBackend(resolveScenario({...}))` in a
//     `beforeEach`, then drive the app and assert. The existing suites mock
//     `@tauri-apps/api/core` at the module level instead, which is fine and is
//     NOT being migrated - this is the seam for NEW tests that want a whole
//     working backend rather than a handful of stubbed commands.
//   - Playwright: `page.addInitScript(installMockBackend, resolveScenario({...}))`.
//
// That second consumer is the reason `installMockBackend` is written the way it
// is. Playwright serializes the function to a string and evaluates it in a fresh
// page context, so its body MUST NOT reference anything from module scope - no
// imports, no constants, no helpers. Everything it needs arrives in its single
// argument. `resolveScenario` therefore does all the merging in Node and hands
// over a plain, JSON-serializable object. Keep that discipline when editing:
// a stray module-scope reference inside the installer body fails only at
// runtime, in the browser, as an opaque ReferenceError.

import {
  ACCOUNTS,
  ACTIVITY_EVENT_TYPES,
  ACTIVITY_PAGE,
  ACTIVITY_SUMMARY,
  ACTIVITY_THROUGHPUT,
  APFS_HELPER_STATUS,
  BACKENDS,
  DRIVE_FOLDER_LISTING,
  EXCLUSION_PREVIEW,
  FILE_VERSIONS,
  MOCK_APP_VERSION,
  RECOVERY_PHRASE,
  RELEASES,
  REMOTE_TREE,
  RESTORE_JOB_RUNNING,
  DRILL_RUNS,
  SCRUB_RUNS,
  SEARCH_HITS,
  SETTINGS,
  SOURCES,
  SOURCE_DOCUMENTS,
  SYNC_STATUS_IDLE,
  TELEMETRY_PREVIEW,
  VERSIONING_CONFIG,
  VSS_HELPER_STATUS,
} from "./fixtures";

// --- Response model -------------------------------------------------------

/** Brand marking a wrapped response, so a raw DTO can never be mistaken for
 * one. This matters: `poll_oauth_status` legitimately resolves to
 * `{ kind: "complete" }`, so discriminating on a bare `kind` field would
 * misread real fixture data as a control object. */
const RESPONSE_BRAND = "__drivenMockResponse";

/** How one command answers: resolve with a value, reject with an error, or
 * never settle (holding a view in its loading state for a screenshot). */
export type MockResponse =
  | { readonly __drivenMockResponse: true; kind: "resolve"; value: unknown }
  | { readonly __drivenMockResponse: true; kind: "reject"; error: unknown }
  | { readonly __drivenMockResponse: true; kind: "pending" };

/** What a scenario may say about a command: a wrapped response, or - for the
 * common case - the resolved value itself. */
export type MockCommandOverride = MockResponse | unknown;

/** Reject this command. `code` is a SPEC s24 dotted error code, which is what
 * `toErrorCode` reads and what the views render as `errors.<code>.long`;
 * passing anything without a `code` makes the UI show `internal.bug`, so pass a
 * real code unless that fallback is the thing under test. */
export function mockError(code: string, message?: string): MockResponse {
  return {
    [RESPONSE_BRAND]: true,
    kind: "reject",
    error: { code, message: message ?? `mock backend error: ${code}` },
  } as MockResponse;
}

/** Never settle this command, so the caller stays in its loading/skeleton
 * state. Use for the loading screenshots. */
export function mockPending(): MockResponse {
  return { [RESPONSE_BRAND]: true, kind: "pending" } as MockResponse;
}

/** Resolve with `value`. Only needed when the value would itself look like a
 * wrapped response; a plain value can be passed directly. */
export function mockValue(value: unknown): MockResponse {
  return { [RESPONSE_BRAND]: true, kind: "resolve", value } as MockResponse;
}

function isWrapped(v: unknown): v is MockResponse {
  return typeof v === "object" && v !== null && RESPONSE_BRAND in v;
}

function wrap(v: MockCommandOverride): MockResponse {
  return isWrapped(v) ? v : ({ [RESPONSE_BRAND]: true, kind: "resolve", value: v } as MockResponse);
}

// --- Scenario -------------------------------------------------------------

/** A scenario: the defaults, plus whatever this test wants to differ. */
export interface MockScenario {
  /** Per-command overrides, keyed by the `#[tauri::command]` name (or a plugin
   * command like `plugin:opener|open_url`). */
  commands?: Record<string, MockCommandOverride>;
  /** Reported by `plugin:app|version`. Defaults to a fixed fake so a release
   * version bump cannot invalidate the visual baselines. */
  appVersion?: string;
}

/** The fully-merged, JSON-serializable scenario handed to `installMockBackend`. */
export interface ResolvedMockScenario {
  responses: Record<string, MockResponse>;
  appVersion: string;
}

/**
 * Every command the app can invoke, with a realistic default answer.
 *
 * The list mirrors `tauri::generate_handler![...]` in `src-tauri/src/lib.rs`
 * plus the plugin commands the app reaches through `@tauri-apps/api`. A command
 * missing from here REJECTS loudly at call time rather than resolving
 * `undefined`, because a silently-undefined answer shows up as a subtly wrong
 * screenshot rather than a failure.
 */
export function defaultCommands(): Record<string, MockCommandOverride> {
  return {
    // --- Plugin commands (@tauri-apps/api + plugins) ---
    "plugin:app|version": MOCK_APP_VERSION,
    "plugin:opener|open_url": undefined,

    // --- Sync (SPEC s11.3) ---
    sync_now: undefined,
    pause_sync: undefined,
    resume_sync: undefined,
    get_pause_state: null,
    get_sync_status: SYNC_STATUS_IDLE,

    // --- Accounts (SPEC s11.1) ---
    list_accounts: ACCOUNTS,
    list_backends: BACKENDS,
    create_s3_account: ACCOUNTS[1],
    create_local_folder_account: ACCOUNTS[0],
    begin_add_account_wizard: "mock-session-1",
    submit_oauth_credentials: undefined,
    start_oauth_signin: { authUrl: "https://accounts.example.com/o/oauth2/auth?mock=1" },
    poll_oauth_status: { kind: "awaitingCallback" },
    cancel_oauth_wizard: undefined,
    finish_add_account: ACCOUNTS[0],
    remove_account: undefined,
    reauth_account: {
      sessionId: "mock-session-reauth",
      authUrl: "https://accounts.example.com/o/oauth2/auth?mock=reauth",
    },

    // --- Sources (SPEC s11.2) ---
    list_sources: SOURCES,
    add_source: { source: SOURCE_DOCUMENTS, recoveryPhrase: null, pendingRecoveryAck: false },
    update_source: SOURCE_DOCUMENTS,
    remove_source: undefined,
    pick_drive_folder: DRIVE_FOLDER_LISTING,
    preview_exclusions: EXCLUSION_PREVIEW,
    preview_exclusions_start: "preview-1",
    preview_exclusions_cancel: undefined,
    get_source_versioning: VERSIONING_CONFIG,
    set_source_versioning: VERSIONING_CONFIG,
    reveal_recovery_phrase: RECOVERY_PHRASE,
    ack_recovery_phrase_saved: SOURCE_DOCUMENTS,

    // --- Backend-owned native dialogs (SPEC s11.6.1) ---
    pick_folder_dialog: { path: "/Users/ada/Documents", token: "mock-path-token" },
    pick_save_zip_dialog: { path: "/Users/ada/Desktop/driven-diagnostics.zip", token: "mock-zip" },

    // --- Settings & misc (SPEC s11.6) ---
    get_settings: SETTINGS,
    update_settings: SETTINGS,
    get_vss_helper_status: VSS_HELPER_STATUS,
    get_apfs_helper_status: APFS_HELPER_STATUS,
    export_diagnostic_bundle: "/Users/ada/Desktop/driven-diagnostics.zip",
    check_for_updates: null,
    list_releases: RELEASES,
    report_frontend_logs: undefined,
    validate_custom_ca: { certCount: 3 },
    validate_proxy: undefined,

    // --- In-app updater (SPEC s15.2) ---
    check_for_update: null,
    install_update: undefined,
    get_update_channel: "stable",
    set_update_channel: "stable",
    get_pending_update_info: null,

    // --- Telemetry (SPEC s16) ---
    get_telemetry_enabled: true,
    set_telemetry_enabled: true,
    get_telemetry_install_id: "00000000-0000-4000-8000-000000000000",
    preview_telemetry_ping: TELEMETRY_PREVIEW,

    // --- Activity (SPEC s11.4) ---
    query_activity: ACTIVITY_PAGE,
    clear_activity_older_than: 0,
    distinct_activity_event_types: ACTIVITY_EVENT_TYPES,
    list_scrub_runs: SCRUB_RUNS,
    list_drill_runs: DRILL_RUNS,
    activity_summary: ACTIVITY_SUMMARY,
    activity_throughput_series: ACTIVITY_THROUGHPUT,

    // --- Restore (SPEC s11.5) ---
    list_remote_tree: REMOTE_TREE,
    search_files: SEARCH_HITS,
    restore_files: "job-1",
    get_restore_job: RESTORE_JOB_RUNNING,
    cancel_restore_job: undefined,
    list_file_versions: FILE_VERSIONS,
  };
}

/**
 * Merge a scenario over the defaults into the plain object `installMockBackend`
 * expects.
 *
 * Runs in Node (or the test process), never in the page, so it is free to
 * import fixtures and use helpers - unlike the installer itself.
 */
export function resolveScenario(scenario: MockScenario = {}): ResolvedMockScenario {
  const merged: Record<string, MockCommandOverride> = {
    ...defaultCommands(),
    ...(scenario.commands ?? {}),
  };
  if (scenario.appVersion !== undefined) {
    merged["plugin:app|version"] = scenario.appVersion;
  }
  const responses: Record<string, MockResponse> = {};
  for (const [cmd, override] of Object.entries(merged)) {
    responses[cmd] = wrap(override);
  }
  return { responses, appVersion: scenario.appVersion ?? MOCK_APP_VERSION };
}

// --- The installed mock ---------------------------------------------------

/** One recorded `invoke`. */
export interface MockCall {
  cmd: string;
  args: Record<string, unknown> | undefined;
}

/** The live control surface of an installed mock. Also published as
 * `window.__drivenMock`, which is how a Playwright spec reaches it
 * (`page.evaluate(() => window.__drivenMock.emit(...))`). */
export interface MockBackendHandle {
  /** Every invoke so far, in order. */
  readonly calls: MockCall[];
  /** Just the calls to `cmd`. */
  callsTo(cmd: string): MockCall[];
  /** Deliver a Rust -> webview event to every live listener. Returns how many
   * listeners were notified, so a test can assert it did not silently fire into
   * the void because the component had not subscribed yet. */
  emit(event: string, payload: unknown): number;
  /** How many live listeners `event` has. */
  listenerCount(event: string): number;
  /** Re-script one command from here on. Accepts a raw value or a wrapped
   * response, exactly like a scenario entry. */
  setResponse(cmd: string, response: MockCommandOverride): void;
  /** Script one command with a FUNCTION, for per-call behaviour (paging,
   * failing only the second attempt, ...). Not serializable, so this is
   * available only in-process: call it from vitest, or from inside a
   * `page.evaluate` in a Playwright spec. Pass null to drop it. */
  setHandler(cmd: string, handler: ((args: Record<string, unknown>) => unknown) | null): void;
  /** Forget recorded calls (keeps the scripted responses). */
  resetCalls(): void;
  /** Remove the globals this installed, restoring whatever was there before. */
  uninstall(): void;
}

declare global {
  interface Window {
    /** Published by `installMockBackend`; how a Playwright spec emits events. */
    __drivenMock?: MockBackendHandle;
    /** The hole in the wall this module fills. `@tauri-apps/api` declares it
     * only in its own internal ambient types, so it is redeclared here (loosely
     * - nothing should be reading fields off it) to keep the installer and the
     * tests that assert against it typed. */
    __TAURI_INTERNALS__?: unknown;
    // `__TAURI_EVENT_PLUGIN_INTERNALS__` is deliberately NOT redeclared:
    // @tauri-apps/api already declares it with a concrete type, and a second
    // (looser) declaration is a hard TS2717 conflict.
  }
}

/**
 * Install the scripted backend onto `window`.
 *
 * SELF-CONTAINED BY CONTRACT: the body references only its argument and
 * built-ins, because Playwright evaluates it as source text in a fresh page.
 * See the module header before adding anything to it.
 */
export function installMockBackend(scenario: ResolvedMockScenario): MockBackendHandle {
  const BRAND = "__drivenMockResponse";

  interface Wrapped {
    kind: "resolve" | "reject" | "pending";
    value?: unknown;
    error?: unknown;
  }

  const responses = new Map<string, Wrapped>(
    Object.entries(scenario.responses) as [string, Wrapped][]
  );
  const handlers = new Map<string, (args: Record<string, unknown>) => unknown>();
  const calls: MockCall[] = [];

  // Callback registry, mirroring Tauri's own: `transformCallback` stores the
  // function and hands the backend a numeric id, which the backend "calls" by
  // invoking `window._<id>(...)`. Keeping the window property too means code
  // that pokes at it the way the real runtime does still works.
  const callbacks = new Map<number, (payload: unknown) => void>();
  let nextCallbackId = 1;

  // Live event listeners: event name -> [{ eventId, callbackId }].
  const listeners = new Map<number, { event: string; callbackId: number }>();
  let nextEventId = 1;

  const prevInternals = (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  const prevEventInternals = (window as unknown as Record<string, unknown>)
    .__TAURI_EVENT_PLUGIN_INTERNALS__;

  function transformCallback(callback: (payload: unknown) => void, once = false): number {
    const id = nextCallbackId++;
    const run = (payload: unknown): void => {
      if (once) {
        callbacks.delete(id);
        delete (window as unknown as Record<string, unknown>)[`_${id}`];
      }
      callback(payload);
    };
    callbacks.set(id, run);
    (window as unknown as Record<string, unknown>)[`_${id}`] = run;
    return id;
  }

  function unregisterCallback(id: number): void {
    callbacks.delete(id);
    delete (window as unknown as Record<string, unknown>)[`_${id}`];
  }

  function invoke(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
    calls.push({ cmd, args });

    // The event plugin is stateful, so it is implemented rather than scripted.
    if (cmd === "plugin:event|listen") {
      const eventId = nextEventId++;
      listeners.set(eventId, {
        event: String(args?.event),
        callbackId: Number(args?.handler),
      });
      return Promise.resolve(eventId);
    }
    if (cmd === "plugin:event|unlisten") {
      listeners.delete(Number(args?.eventId));
      return Promise.resolve(undefined);
    }
    if (cmd === "plugin:event|emit" || cmd === "plugin:event|emit_to") {
      return Promise.resolve(undefined);
    }

    const handler = handlers.get(cmd);
    if (handler) {
      try {
        return Promise.resolve(handler(args ?? {}));
      } catch (e) {
        return Promise.reject(e);
      }
    }

    const scripted = responses.get(cmd);
    if (!scripted) {
      // Loud on purpose. An unscripted command that quietly resolved undefined
      // would render a plausible-looking but wrong screenshot, which is worse
      // than a failing test.
      return Promise.reject({
        code: "internal.bug",
        message: `mock backend: no response scripted for command "${cmd}"`,
      });
    }
    if (scripted.kind === "reject") return Promise.reject(scripted.error);
    if (scripted.kind === "pending") return new Promise<unknown>(() => {});
    return Promise.resolve(scripted.value);
  }

  const handle: MockBackendHandle = {
    calls,
    callsTo(cmd: string) {
      return calls.filter((c) => c.cmd === cmd);
    },
    emit(event: string, payload: unknown): number {
      let notified = 0;
      for (const [eventId, reg] of listeners) {
        if (reg.event !== event) continue;
        const cb = callbacks.get(reg.callbackId);
        if (!cb) continue;
        cb({ event, id: eventId, payload });
        notified++;
      }
      return notified;
    },
    listenerCount(event: string): number {
      let n = 0;
      for (const reg of listeners.values()) if (reg.event === event) n++;
      return n;
    },
    setResponse(cmd: string, response: MockCommandOverride): void {
      handlers.delete(cmd);
      const wrapped =
        typeof response === "object" && response !== null && BRAND in response
          ? (response as unknown as Wrapped)
          : ({ kind: "resolve", value: response } as Wrapped);
      responses.set(cmd, wrapped);
    },
    setHandler(cmd, handlerFn): void {
      if (handlerFn) handlers.set(cmd, handlerFn);
      else handlers.delete(cmd);
    },
    resetCalls(): void {
      calls.length = 0;
    },
    uninstall(): void {
      for (const id of callbacks.keys()) {
        delete (window as unknown as Record<string, unknown>)[`_${id}`];
      }
      callbacks.clear();
      listeners.clear();
      const w = window as unknown as Record<string, unknown>;
      w.__TAURI_INTERNALS__ = prevInternals;
      w.__TAURI_EVENT_PLUGIN_INTERNALS__ = prevEventInternals;
      delete w.__drivenMock;
    },
  };

  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    invoke,
    transformCallback,
    unregisterCallback,
    // `convertFileSrc` is unused by the app today, but the real object has it
    // and an asset URL is cheap to answer honestly.
    convertFileSrc: (filePath: string, protocol = "asset") => `${protocol}://localhost/${filePath}`,
  };
  // `_unlisten` in @tauri-apps/api/event calls this BEFORE its invoke and does
  // not guard it, so without this stub every component unmount - i.e. every
  // route change - throws a TypeError.
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener: (_event: string, eventId: number) => {
      listeners.delete(Number(eventId));
    },
  };
  (window as unknown as Record<string, unknown>).__drivenMock = handle;

  return handle;
}

/** Convenience for vitest: resolve a scenario and install it in one call. */
export function installScenario(scenario: MockScenario = {}): MockBackendHandle {
  return installMockBackend(resolveScenario(scenario));
}
