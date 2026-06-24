import { defineStore } from "pinia";
import { computed, ref } from "vue";

import * as ipc from "../ipc/commands";
import { useSourcesStore } from "./sources";
import type {
  AddAccountWizardSessionId,
  AddSourceRequest,
  OAuthStatus,
  SessionId,
} from "../ipc/types";

// Setup-wizard store (DESIGN s8.5 5-step wizard; SPEC s11.1 OAuth flow). Holds
// the wizard step cursor + the in-flight OAuth session/state + the staged
// first-source inputs + the encryption opt-in. The view drives the OAuth IPC
// sequence (begin -> submitCredentials -> startSignin -> open auth URL ->
// poll/await oauth:complete -> finish) through these actions; the store keeps
// the cross-step state so back/next never loses the user's input.
//
// Polling note: we never spin a busy until-loop. The view drives completion via
// the `oauth:complete` event (one-shot `poll()` on fire) plus a manual "I'm
// done signing in" poll button mirroring SPEC s6.1 step 7's "Done - continue"
// affordance. Both paths funnel through the single `poll()` action below.

/** DESIGN s8.5 wizard steps, 1-indexed to match the design. */
export const WIZARD_STEPS = [
  "welcome",
  "credentials",
  "source",
  "encryption",
  "confirm",
] as const;
export type WizardStep = (typeof WIZARD_STEPS)[number];

export const useSetupStore = defineStore("setup", () => {
  const stepIndex = ref(0);
  const session = ref<AddAccountWizardSessionId | null>(null);
  const oauthStatus = ref<OAuthStatus | null>(null);

  // Cross-step staged inputs. Kept here (not component-local) so navigating
  // back/forward preserves what the user already entered.
  const accountId = ref<string | null>(null);
  const accountEmail = ref<string | null>(null);
  const localPath = ref<string | null>(null);
  // C1: the one-shot dialog token for the chosen local folder (proves the path
  // came from the backend folder dialog). Required by add_source.
  const localPathToken = ref<string | null>(null);
  const driveFolderId = ref<string | null>(null);
  const driveFolderPath = ref<string>("");
  const sourceDisplayName = ref<string>("");
  const encryptionEnabled = ref(false);
  // B3: the 24-word BIP39 phrase the backend RETURNS from add_source on the
  // first encrypted source. Held only in memory; revealed once via
  // RecoveryPhraseReveal, never persisted. `null` until the source is created.
  const recoveryPhrase = ref<string[] | null>(null);
  // B3: the user has acknowledged saving the recovery phrase. Finish is gated
  // on this whenever a phrase was actually displayed.
  const phraseAcknowledged = ref(false);
  const sourceId = ref<string | null>(null);

  // Transient UX flags surfaced by the view.
  const busy = ref(false);
  // Stable error code (SPEC s24) the view maps via t(`errors.${code}.long`).
  const errorCode = ref<string | null>(null);

  const step = computed<WizardStep>(() => WIZARD_STEPS[stepIndex.value]);
  const canGoBack = computed(() => stepIndex.value > 0);
  const canGoNext = computed(() => stepIndex.value < WIZARD_STEPS.length - 1);

  /** True once the OAuth sign-in has resolved to `complete`. */
  const signedIn = computed(() => oauthStatus.value?.kind === "complete");

  /** B3: a recovery phrase was returned (the source's encryption opt-in
   * generated the master key), so the user MUST see + acknowledge it. */
  const hasRecoveryPhrase = computed(
    () => (recoveryPhrase.value?.length ?? 0) > 0,
  );

  /** B3: the wizard may Finish only once any displayed recovery phrase has been
   * acknowledged. With no phrase (unencrypted), Finish is always allowed. */
  const canFinish = computed(
    () => !hasRecoveryPhrase.value || phraseAcknowledged.value,
  );

  /** B3: the source has been created (so the confirm step can show the phrase
   * reveal + the start-sync affordance). */
  const sourceCreated = computed(() => sourceId.value !== null);

  function next(): void {
    if (canGoNext.value) stepIndex.value += 1;
  }

  function back(): void {
    if (canGoBack.value) stepIndex.value -= 1;
  }

  function clearError(): void {
    errorCode.value = null;
  }

  async function begin(): Promise<void> {
    session.value = await ipc.beginAddAccountWizard();
  }

  async function submitCredentials(
    clientId: string,
    clientSecret: string,
  ): Promise<void> {
    const s = requireSession();
    await ipc.submitOauthCredentials(s, clientId, clientSecret);
  }

  async function startSignin(): Promise<string> {
    const s = requireSession();
    const { authUrl } = await ipc.startOauthSignin(s);
    return authUrl;
  }

  async function poll(): Promise<OAuthStatus> {
    const s = requireSession();
    const status = await ipc.pollOauthStatus(s);
    oauthStatus.value = status;
    return status;
  }

  async function finish(displayName: string | null): Promise<void> {
    const s = requireSession();
    const account = await ipc.finishAddAccount(s, displayName);
    accountId.value = account.id;
    accountEmail.value = account.email;
  }

  /**
   * Drive the full credential -> sign-in handoff for the credentials step:
   * submit the pasted client id/secret, start the loopback OAuth flow, and hand
   * the returned auth URL to `openUrl` (defaults to the webview `window.open`,
   * overridable for tests). The browser round-trip then resolves via the
   * `oauth:complete` event or a manual `poll()`.
   */
  async function connectAccount(
    clientId: string,
    clientSecret: string,
    openUrl: (url: string) => void = defaultOpenUrl,
  ): Promise<void> {
    busy.value = true;
    errorCode.value = null;
    try {
      if (!session.value) {
        await begin();
      }
      await submitCredentials(clientId, clientSecret);
      const authUrl = await startSignin();
      openUrl(authUrl);
      oauthStatus.value = { kind: "awaitingCallback" };
    } catch (e) {
      errorCode.value = toErrorCode(e);
      throw e;
    } finally {
      busy.value = false;
    }
  }

  /**
   * One-shot completion check used by both the `oauth:complete` event handler
   * and the manual "I'm done" button. Refreshes status, records any failure
   * code, and advances to the source step on success. Returns whether sign-in
   * is now complete. No looping - the caller decides when to re-check.
   */
  async function checkSigninComplete(): Promise<boolean> {
    busy.value = true;
    try {
      const status = await poll();
      if (status.kind === "complete") {
        await finish(null);
        return true;
      }
      if (status.kind === "failed") {
        errorCode.value = status.code;
      }
      return false;
    } catch (e) {
      errorCode.value = toErrorCode(e);
      return false;
    } finally {
      busy.value = false;
    }
  }

  /**
   * Create the first backup source from the staged inputs (DESIGN s8.5 step 3).
   * Reuses the sources store so the new source lands in the same reactive list
   * the Sources tab renders. The encryption flag rides along; the backend
   * returns the recovery phrase out-of-band on opt-in (surfaced in step 4 via
   * RecoveryPhraseReveal). Requires a chosen account + dialog-derived local
   * path + a picked Drive destination.
   */
  async function createFirstSource(): Promise<void> {
    // R1-P2-3: idempotent. The one-shot folder dialog token is CONSUMED by the
    // backend on the first add_source, so re-entering the encryption step (Back
    // from confirm, then Next again) must NOT re-call add_source - it would fail
    // with a stale/consumed token and wedge the wizard. If the source already
    // exists, this is a no-op (the staged phrase + ack state are preserved).
    if (sourceId.value !== null) {
      return;
    }
    busy.value = true;
    errorCode.value = null;
    try {
      const acct = accountId.value;
      const local = localPath.value;
      const token = localPathToken.value;
      const drive = driveFolderId.value;
      if (!acct || !local || !token || !drive) {
        throw new Error("source step is incomplete");
      }
      const req: AddSourceRequest = {
        accountId: acct,
        displayName: sourceDisplayName.value || driveFolderPath.value || local,
        localPathToken: token,
        localPath: local,
        driveFolderId: drive,
        driveFolderPath: driveFolderPath.value,
        encryptionEnabled: encryptionEnabled.value,
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
      };
      const sources = useSourcesStore();
      const result = await sources.add(req);
      sourceId.value = result.source.id;
      // B3: capture the one-time recovery phrase (present only when this opt-in
      // generated the account master key). Reset the ack so the confirm step
      // gates Finish until the user attests they saved the words.
      recoveryPhrase.value = result.recoveryPhrase;
      phraseAcknowledged.value = false;
    } catch (e) {
      errorCode.value = toErrorCode(e);
      throw e;
    } finally {
      busy.value = false;
    }
  }

  /** B3: record the user's acknowledgement that they saved the recovery phrase
   * (gates Finish on the confirm step). */
  function acknowledgePhrase(value: boolean): void {
    phraseAcknowledged.value = value;
  }

  /**
   * Kick off the initial sync for the just-created source (DESIGN s8.5 step 5).
   * Scoped to the new source so the wizard never triggers a global sweep.
   */
  async function startInitialSync(): Promise<void> {
    busy.value = true;
    errorCode.value = null;
    try {
      const sid = sourceId.value;
      if (!sid) {
        throw new Error("no source to sync");
      }
      await ipc.syncNow(sid);
    } catch (e) {
      errorCode.value = toErrorCode(e);
      throw e;
    } finally {
      busy.value = false;
    }
  }

  function reset(): void {
    stepIndex.value = 0;
    session.value = null;
    oauthStatus.value = null;
    accountId.value = null;
    accountEmail.value = null;
    localPath.value = null;
    localPathToken.value = null;
    driveFolderId.value = null;
    driveFolderPath.value = "";
    sourceDisplayName.value = "";
    encryptionEnabled.value = false;
    recoveryPhrase.value = null;
    phraseAcknowledged.value = false;
    sourceId.value = null;
    busy.value = false;
    errorCode.value = null;
  }

  function requireSession(): SessionId {
    if (!session.value) {
      throw new Error("setup wizard session not started");
    }
    return session.value;
  }

  return {
    stepIndex,
    step,
    session,
    oauthStatus,
    accountId,
    accountEmail,
    localPath,
    localPathToken,
    driveFolderId,
    driveFolderPath,
    sourceDisplayName,
    encryptionEnabled,
    recoveryPhrase,
    phraseAcknowledged,
    sourceId,
    busy,
    errorCode,
    canGoBack,
    canGoNext,
    signedIn,
    hasRecoveryPhrase,
    canFinish,
    sourceCreated,
    next,
    back,
    clearError,
    begin,
    submitCredentials,
    startSignin,
    poll,
    finish,
    connectAccount,
    checkSigninComplete,
    createFirstSource,
    acknowledgePhrase,
    startInitialSync,
    reset,
  };
});

/**
 * Default browser opener. In a Tauri webview `window.open` is intercepted and
 * routed to the system browser; this is the dependency the credentials step
 * overrides in tests so no real window is opened.
 */
function defaultOpenUrl(url: string): void {
  if (typeof window !== "undefined" && typeof window.open === "function") {
    window.open(url, "_blank");
  }
}

/**
 * Map a rejected IPC error onto a stable SPEC s24 code. Tauri serializes the
 * `{ code, message, ... }` shape (SPEC s24); we read `.code` when present and
 * fall back to `internal.bug` so the view always has a translatable key.
 */
function toErrorCode(e: unknown): string {
  if (e && typeof e === "object" && "code" in e) {
    const code = (e as { code: unknown }).code;
    if (typeof code === "string" && code.length > 0) {
      return code;
    }
  }
  return "internal.bug";
}
