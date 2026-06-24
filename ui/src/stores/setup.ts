import { defineStore } from "pinia";
import { computed, ref } from "vue";

import * as ipc from "../ipc/commands";
import type {
  AddAccountWizardSessionId,
  OAuthStatus,
  SessionId,
} from "../ipc/types";

// Setup-wizard store (DESIGN s8.5 5-step wizard; SPEC s11.1 OAuth flow). Holds
// the wizard step cursor + the in-flight OAuth session/state. M6 scaffold: the
// step model + action SIGNATURES are frozen; the accounts implementer fills in
// the richer flow (polling cadence, error surfacing, encryption opt-in handoff
// to the recovery-phrase reveal).

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

  const step = computed<WizardStep>(() => WIZARD_STEPS[stepIndex.value]);
  const canGoBack = computed(() => stepIndex.value > 0);
  const canGoNext = computed(() => stepIndex.value < WIZARD_STEPS.length - 1);

  function next(): void {
    if (canGoNext.value) stepIndex.value += 1;
  }

  function back(): void {
    if (canGoBack.value) stepIndex.value -= 1;
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
    await ipc.finishAddAccount(s, displayName);
  }

  function reset(): void {
    stepIndex.value = 0;
    session.value = null;
    oauthStatus.value = null;
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
    canGoBack,
    canGoNext,
    next,
    back,
    begin,
    submitCredentials,
    startSignin,
    poll,
    finish,
    reset,
  };
});
