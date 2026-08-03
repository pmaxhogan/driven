import { defineStore } from "pinia";
import { computed, ref } from "vue";
import { openUrl } from "@tauri-apps/plugin-opener";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { useSourcesStore } from "./sources";
import type {
  AddAccountWizardSessionId,
  AddSourceRequest,
  BackendDto,
  BackendKindId,
  CreateS3AccountRequest,
  CreateLocalFolderAccountRequest,
  CreateSftpAccountRequest,
  OAuthStatus,
  SessionId,
} from "../ipc/types";
import { DEFAULT_BACKEND_ID } from "../ipc/types";

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
export const WIZARD_STEPS = ["welcome", "credentials", "source", "encryption", "confirm"] as const;
export type WizardStep = (typeof WIZARD_STEPS)[number];

export const useSetupStore = defineStore("setup", () => {
  const stepIndex = ref(0);
  const session = ref<AddAccountWizardSessionId | null>(null);
  const oauthStatus = ref<OAuthStatus | null>(null);
  // The loopback consent URL returned by start_oauth_signin. Held so the
  // credentials step can offer a manual "open / copy this link" fallback when the
  // automatic system-browser open is blocked - a failed auto-open is never a dead
  // end. Cleared by reset(); set each time connectAccount starts a sign-in.
  const authUrl = ref<string | null>(null);

  // Destination (backend) selection. `backends` is whatever the RUST FACTORY
  // reports it can construct - never a hard-coded UI list - so the picker can
  // only offer destinations this build actually implements. `backendId` is the
  // user's choice; it defaults to Google Drive so a wizard run that never
  // reaches the picker (or a build with a single destination) behaves exactly as
  // it always did.
  const backends = ref<BackendDto[]>([]);
  const backendId = ref<BackendKindId>(DEFAULT_BACKEND_ID);
  // The destination ROOT of a local-folder account, kept for display on the
  // source step (which shows the resolved `<root>/<sub-folder>` the backup will
  // land in). Not sent anywhere - the account row already holds it.
  const localFolderRoot = ref<string | null>(null);
  // The SSH (SFTP) creation probe's TOFU (trust-on-first-use) result, kept for
  // display on the source step (step 2's own card is unreachable once
  // `onSftpSubmit` advances past it on success - see `SftpAccountCreatedDto`'s
  // doc). `sftpHostKeyFingerprint` is the pinned host key so the user can see
  // what was trusted; `sftpAdopted` is whether the root already carried
  // another Driven destination marker that this probe reused rather than
  // stamping fresh (so every object already there stays reachable).
  const sftpHostKeyFingerprint = ref<string | null>(null);
  const sftpAdopted = ref(false);

  // Cross-step staged inputs. Kept here (not component-local) so navigating
  // back/forward preserves what the user already entered.
  const accountId = ref<string | null>(null);
  const accountEmail = ref<string | null>(null);
  const localPath = ref<string | null>(null);
  // C1: the one-shot dialog token for the chosen local folder (proves the path
  // came from the backend folder dialog). Required by add_source.
  const localPathToken = ref<string | null>(null);
  const driveFolderId = ref<string | null>(null);
  // Issue #7: the Google Shared Drive id the destination lives in (null = My
  // Drive), published by the DriveFolderPicker and persisted with the source.
  const driveId = ref<string | null>(null);
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
  // R3-P1-1: the user has actually REVEALED the phrase at least once. Finish is
  // gated on reveal AND acknowledge so a user can never attest "I saved it" while
  // the phrase was still hidden (which would risk starting unrecoverable
  // encrypted backups). Reset alongside `phraseAcknowledged` whenever the phrase
  // changes/clears.
  const phraseRevealed = ref(false);
  const sourceId = ref<string | null>(null);
  // M9c D4 (M6 R4-P1-1, DATA-SAFETY): true when the created first source was
  // persisted DISABLED and awaits a backend recovery-phrase ack. Finish then calls
  // ackRecoveryPhrase (which ENABLES the source) before starting the initial sync;
  // the reveal button calls revealRecoveryPhrase (the backend reveal the ack gate
  // requires).
  const pendingRecoveryAck = ref(false);

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
  const hasRecoveryPhrase = computed(() => (recoveryPhrase.value?.length ?? 0) > 0);

  /** B3 + R3-P1-1: the wizard may Finish only once any displayed recovery phrase
   * has been REVEALED and acknowledged. With no phrase (unencrypted), Finish is
   * always allowed. Gating on reveal (not just acknowledge) blocks a user from
   * ticking the confirm box while the phrase is still hidden. */
  const canFinish = computed(
    () => !hasRecoveryPhrase.value || (phraseRevealed.value && phraseAcknowledged.value)
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

  /** The descriptor for the currently-selected destination, if it is one the
   * backend advertised. `undefined` until `loadBackends` has resolved. */
  const selectedBackend = computed<BackendDto | undefined>(() =>
    backends.value.find((b) => b.id === backendId.value)
  );

  /** Whether the selected destination authenticates through the OAuth consent
   * step. Defaults to `true` (the Drive behaviour) until the descriptors load,
   * so the wizard never briefly hides the credentials step it is about to need. */
  const backendUsesOauth = computed(() => selectedBackend.value?.usesOauth ?? true);

  /** Whether the selected destination has a browsable remote tree, i.e. whether
   * the source step should offer a destination picker at all. Defaults to `true`
   * (the Drive behaviour) until the descriptors load, so the step never briefly
   * hides the picker it is about to need.
   *
   * A local folder does NOT: its destination root was already chosen with the OS
   * folder dialog, and browsing below it would only offer the user a way to nest
   * one backup inside another.
   *
   * This - NOT `backendUsesOauth` - is the seam that decides whether a picker
   * renders. Both are false for a local folder, which is why conflating them is
   * tempting and wrong: S3 does not use OAuth but DOES browse (its "folders" are
   * key prefixes), so gating the picker on OAuth would silently delete per-source
   * prefix selection from the S3 destination. */
  const backendSupportsFolderPicker = computed(
    () => selectedBackend.value?.supportsFolderPicker ?? true
  );

  /** Whether the source step has a destination to save.
   *
   * The predicate is `!== null`, NEVER truthiness: the EMPTY STRING is a real,
   * selectable destination id - it is the root of any backend whose root is the
   * empty prefix (an S3 bucket root). `!!driveFolderId` treated that legitimate
   * selection as "nothing picked", so an S3 account's Next button stayed disabled
   * forever and first-run setup could not be completed at all.
   *
   * A destination with no browsable tree has nothing to pick on this step - its
   * destination was fixed when the account was created - so it is satisfied. */
  const destinationSelected = computed(
    () => !backendSupportsFolderPicker.value || driveFolderId.value !== null
  );

  /** Load the destinations this build can construct. Idempotent and safe to call
   * on every wizard entry. On failure the list stays empty and the wizard runs
   * with the Google Drive default rather than dead-ending. */
  async function loadBackends(): Promise<void> {
    try {
      // Defensive coercion: an older backend that does not implement
      // `list_backends` resolves undefined rather than rejecting, and a
      // non-array would break every consumer of `backends`. Either way the
      // wizard falls through to the Google Drive default.
      const list = await ipc.listBackends();
      backends.value = Array.isArray(list) ? list : [];
      // Keep the selection valid: if the current choice is not offered (a
      // stale value, or a first load), fall back to the advertised default.
      if (backends.value.length > 0 && !backends.value.some((b) => b.id === backendId.value)) {
        backendId.value = backends.value.find((b) => b.isDefault)?.id ?? DEFAULT_BACKEND_ID;
      }
    } catch (e) {
      errorCode.value = toErrorCode(e);
    }
  }

  /** Choose the backup destination for this wizard run. Rejected server-side if
   * the id is not one this build implements. */
  function selectBackend(id: BackendKindId): void {
    backendId.value = id;
  }

  async function begin(): Promise<void> {
    session.value = await ipc.beginAddAccountWizard(backendId.value);
  }

  /**
   * Create an S3-backed account from the credentials step's form (the non-OAuth
   * branch). One call: the backend validates the settings, PROVES the key pair
   * can reach the bucket, stores the secret in the OS keychain, writes the
   * account row and hot-spawns its orchestrator - so on success the wizard is in
   * exactly the state the OAuth path reaches after `finish`.
   */
  async function createS3Account(req: CreateS3AccountRequest): Promise<boolean> {
    busy.value = true;
    errorCode.value = null;
    try {
      const account = await ipc.createS3Account(req);
      accountId.value = account.id;
      accountEmail.value = account.email;
      // A stale SFTP fingerprint/adoption note from an abandoned attempt on
      // this wizard run (Back, then a different backend) must not survive
      // onto THIS account's source step - see the step-3 template's own
      // backendId gate for the belt half of this guard.
      sftpHostKeyFingerprint.value = null;
      sftpAdopted.value = false;
      // The source step gates on a chosen destination; an S3 account has no
      // consent round trip, so mark the sign-in resolved here.
      oauthStatus.value = { kind: "complete" };
      return true;
    } catch (e) {
      errorCode.value = toErrorCode(e);
      return false;
    } finally {
      busy.value = false;
    }
  }

  /**
   * Create a local-folder-backed account from the destination step's form (the
   * non-OAuth branch). One call: the backend validates the folder, PROVES it is
   * writable, stamps its destination marker, writes the account row and
   * hot-spawns its orchestrator - so on success the wizard is in exactly the
   * state the OAuth path reaches after `finish`.
   */
  async function createLocalFolderAccount(req: CreateLocalFolderAccountRequest): Promise<boolean> {
    busy.value = true;
    errorCode.value = null;
    try {
      const account = await ipc.createLocalFolderAccount(req);
      accountId.value = account.id;
      accountEmail.value = account.email;
      localFolderRoot.value = req.root;
      // See `createS3Account`'s identical guard: clears a stale SFTP
      // fingerprint/adoption note from an abandoned attempt earlier in this
      // wizard run.
      sftpHostKeyFingerprint.value = null;
      sftpAdopted.value = false;
      // The source step gates on a resolved destination; a local folder has no
      // consent round trip, so mark the sign-in resolved here.
      oauthStatus.value = { kind: "complete" };
      return true;
    } catch (e) {
      errorCode.value = toErrorCode(e);
      return false;
    } finally {
      busy.value = false;
    }
  }

  /**
   * Create an SSH (SFTP) backed account from the credentials step's form (the
   * non-OAuth, non-local, non-S3 branch). One call: the backend validates the
   * settings, PROVES a real SSH session can reach the server and the root path
   * with a real write, PINS the host key (trust on first use), stores the
   * secret in the OS keychain and writes the account row - so on success the
   * wizard is in exactly the state the OAuth path reaches after `finish`.
   *
   * Unlike `createS3Account` / `createLocalFolderAccount`, the response is NOT
   * a bare `AccountDto` - `create_sftp_account` returns `SftpAccountCreatedDto`
   * (`{ account, hostKeyFingerprint, adopted }`), so this unwraps `.account`
   * and records the fingerprint + adoption flag for the source step to display
   * (step 2's own card is unreachable by the time it would need to show them -
   * this action advances the wizard past it on success).
   */
  async function createSftpAccount(req: CreateSftpAccountRequest): Promise<boolean> {
    busy.value = true;
    errorCode.value = null;
    try {
      const result = await ipc.createSftpAccount(req);
      accountId.value = result.account.id;
      accountEmail.value = result.account.email;
      sftpHostKeyFingerprint.value = result.hostKeyFingerprint;
      sftpAdopted.value = result.adopted;
      // The source step gates on a chosen destination; SFTP has no consent
      // round trip, so mark the sign-in resolved here.
      oauthStatus.value = { kind: "complete" };
      return true;
    } catch (e) {
      errorCode.value = toErrorCode(e);
      return false;
    } finally {
      busy.value = false;
    }
  }

  /**
   * The destination sub-folder id to back a local source up into, derived from
   * the source folder's own name.
   *
   * A destination with no browsable tree still needs a non-empty `driveFolderId`
   * on the source row (`add_source` requires one, and rejects whitespace in it -
   * it was written for Drive's opaque ids). Rather than invent an opaque handle,
   * this mirrors what a Drive user would have picked by hand: a folder named
   * after the source, so `~/Documents` lands in `<destination>/Documents/` and
   * the backup stays browsable and hand-restorable, which is most of the point
   * of backing up to a folder you own.
   *
   * Whitespace collapses to `-` to satisfy the id validator. Two sources whose
   * names collapse to one id would share a destination sub-tree - exactly what
   * happens on Drive when a user picks the same destination folder twice, and
   * visible on the source step, which shows the resolved destination path.
   */
  function destinationFolderIdFor(localPathValue: string): string {
    const parts = localPathValue.split(/[\\/]/).filter(Boolean);
    // A path with no components at all (a bare root) has no name to borrow, so
    // fall back to a fixed one rather than emitting a separator - a destination
    // folder id must be a single component.
    const base = parts.length > 0 ? parts[parts.length - 1] : "";
    const slug = base.replace(/\s+/g, "-");
    return slug.length > 0 ? `${slug}/` : "backup/";
  }

  async function submitCredentials(clientId: string, clientSecret: string): Promise<void> {
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
   * the returned auth URL to `open` (defaults to the Tauri opener plugin, which
   * opens the system browser reliably; `window.open` in a Tauri v2 webview does
   * NOT reliably reach the system browser, so the consent page never opened).
   * The opener is awaited and may be async; it stays injectable so unit tests can
   * stub it. The browser round-trip then resolves via the `oauth:complete` event
   * or a manual `poll()`.
   *
   * The auth URL is captured into `authUrl` BEFORE the open is attempted and the
   * status is moved to `awaitingCallback` (the loopback server is already
   * listening), so even if the auto-open fails the credentials step can offer a
   * manual "open / copy this link" fallback - a failed open is recoverable, not a
   * dead end. An open failure records the dedicated `auth.browser_open_failed`
   * code (the view tells the user to use the link) rather than throwing.
   */
  async function connectAccount(
    clientId: string,
    clientSecret: string,
    open: (url: string) => void | Promise<void> = defaultOpenUrl
  ): Promise<void> {
    busy.value = true;
    errorCode.value = null;
    try {
      if (!session.value) {
        await begin();
      }
      await submitCredentials(clientId, clientSecret);
      const url = await startSignin();
      authUrl.value = url;
      oauthStatus.value = { kind: "awaitingCallback" };
      try {
        await open(url);
      } catch {
        // The auto-open failed (no system browser, blocked, opener error). The
        // loopback server is still listening and `authUrl` is captured, so the
        // user can finish via the manual fallback. Surface a clear code instead
        // of wedging on a silent "waiting" state.
        errorCode.value = "auth.browser_open_failed";
      }
    } catch (e) {
      errorCode.value = toErrorCode(e);
      throw e;
    } finally {
      busy.value = false;
    }
  }

  /**
   * Re-open (or first-open) the captured consent URL via the system browser,
   * powering the credentials step's manual "open the sign-in page" fallback.
   * Injectable opener (defaults to the Tauri opener plugin) so it stays testable.
   * Clears any prior error on success; records `auth.browser_open_failed` if the
   * open is still blocked.
   */
  async function openAuthUrl(
    open: (url: string) => void | Promise<void> = defaultOpenUrl
  ): Promise<void> {
    const url = authUrl.value;
    if (!url) return;
    try {
      await open(url);
      errorCode.value = null;
    } catch {
      errorCode.value = "auth.browser_open_failed";
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
   * path + a destination that is valid for the account's backend
   * (`destinationSelected`).
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
      // Truthiness is WRONG for the destination id: "" is the bucket root of an
      // S3-shaped destination, a real selection. Gate on the shared
      // `destinationSelected` predicate so this and the view's Next button can
      // never disagree about what "picked a destination" means.
      if (!acct || !local || !token || !destinationSelected.value) {
        throw new Error("source step is incomplete");
      }
      const req: AddSourceRequest = {
        accountId: acct,
        displayName: sourceDisplayName.value || driveFolderPath.value || local,
        localPathToken: token,
        localPath: local,
        // A destination with no browsable tree contributed no id; its root is the
        // empty one, which is exactly what the backend accepts for it.
        driveFolderId: driveFolderId.value ?? "",
        driveId: driveId.value,
        driveFolderPath: driveFolderPath.value,
        encryptionEnabled: encryptionEnabled.value,
        respectGitignore: true,
        includePatterns: [],
        excludePatterns: [],
      };
      const sources = useSourcesStore();
      const result = await sources.add(req);
      sourceId.value = result.source.id;
      // M9c D4: a pending-ack source was persisted DISABLED; Finish calls the
      // backend ack (which enables it).
      pendingRecoveryAck.value = result.pendingRecoveryAck;
      // B3: capture the one-time recovery phrase (present only when this opt-in
      // generated the account master key). Reset the ack so the confirm step
      // gates Finish until the user attests they saved the words.
      recoveryPhrase.value = result.recoveryPhrase;
      phraseAcknowledged.value = false;
      // R3-P1-1: a fresh phrase must be revealed before it can be acknowledged.
      phraseRevealed.value = false;
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

  /** R3-P1-1: record that the user has revealed the phrase (or that the reveal
   * was re-locked because the phrase changed). When the reveal is cleared, the
   * acknowledgement is force-cleared too so Finish cannot stay enabled. */
  function markPhraseRevealed(value: boolean): void {
    phraseRevealed.value = value;
    if (!value) {
      phraseAcknowledged.value = false;
    }
  }

  /** M9c D4: the BACKEND reveal action (the ack gate depends on it). Only
   * meaningful for a pending-ack source. Returns the words for display. */
  async function revealRecoveryPhrase(): Promise<string[]> {
    const sid = sourceId.value;
    if (!sid) throw new Error("no source to reveal a recovery phrase for");
    const sources = useSourcesStore();
    return sources.revealRecoveryPhrase(sid);
  }

  /** M9c D4: acknowledge the recovery phrase was saved, ENABLING the first
   * encrypted source. Rejected by the backend unless a real reveal was recorded.
   * Called by Finish before the initial sync when the source is pending-ack. */
  async function ackRecoveryPhrase(): Promise<void> {
    const sid = sourceId.value;
    if (!sid) throw new Error("no source to acknowledge");
    const sources = useSourcesStore();
    await sources.ackRecoveryPhrase(sid);
    pendingRecoveryAck.value = false;
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
    authUrl.value = null;
    // The destination list is a property of the BUILD, not of a wizard run, so
    // it survives a reset; only the user's choice is reverted to the default.
    backendId.value = backends.value.find((b) => b.isDefault)?.id ?? DEFAULT_BACKEND_ID;
    localFolderRoot.value = null;
    sftpHostKeyFingerprint.value = null;
    sftpAdopted.value = false;
    accountId.value = null;
    accountEmail.value = null;
    localPath.value = null;
    localPathToken.value = null;
    driveFolderId.value = null;
    driveId.value = null;
    driveFolderPath.value = "";
    sourceDisplayName.value = "";
    encryptionEnabled.value = false;
    recoveryPhrase.value = null;
    phraseAcknowledged.value = false;
    phraseRevealed.value = false;
    sourceId.value = null;
    pendingRecoveryAck.value = false;
    busy.value = false;
    errorCode.value = null;
  }

  /** R4-P2-4: abandon the wizard - tell the backend to drop the in-flight OAuth
   * session (clearing its BYO creds + tokens from the server-side registry),
   * then reset local state. Best-effort: a backend error is swallowed (the TTL
   * sweep reaps an unreachable session anyway), and the local reset always runs.
   * Idempotent - safe if the session was already consumed by `finish`. */
  async function cancel(): Promise<void> {
    const s = session.value;
    if (s) {
      try {
        await ipc.cancelOauthWizard(s);
      } catch {
        // Non-fatal: the backend TTL sweep reaps an abandoned session.
      }
    }
    reset();
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
    authUrl,
    backends,
    backendId,
    selectedBackend,
    localFolderRoot,
    sftpHostKeyFingerprint,
    sftpAdopted,
    backendUsesOauth,
    backendSupportsFolderPicker,
    destinationSelected,
    loadBackends,
    selectBackend,
    createS3Account,
    createLocalFolderAccount,
    createSftpAccount,
    destinationFolderIdFor,
    accountId,
    accountEmail,
    localPath,
    localPathToken,
    driveFolderId,
    driveId,
    driveFolderPath,
    sourceDisplayName,
    encryptionEnabled,
    recoveryPhrase,
    phraseAcknowledged,
    phraseRevealed,
    sourceId,
    pendingRecoveryAck,
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
    openAuthUrl,
    checkSigninComplete,
    createFirstSource,
    acknowledgePhrase,
    markPhraseRevealed,
    revealRecoveryPhrase,
    ackRecoveryPhrase,
    startInitialSync,
    reset,
    cancel,
  };
});

/**
 * Default browser opener. Uses the official Tauri v2 opener plugin, which opens
 * the system default browser reliably from the webview. (The previous
 * `window.open(url, "_blank")` does NOT reliably reach the system browser in a
 * Tauri v2 webview, so the OAuth consent page never opened - the sign-in bug.)
 * Awaited by callers; rejects if the open fails so they can fall back to the
 * manual link. This is the dependency the credentials step overrides in tests so
 * no real browser is opened.
 */
async function defaultOpenUrl(url: string): Promise<void> {
  await openUrl(url);
}
