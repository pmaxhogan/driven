// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import RecoveryPhraseReveal from "../components/RecoveryPhraseReveal.vue";

// R3-P1-1: the recovery-phrase reveal gates the acknowledge checkbox on the
// phrase having actually been REVEALED, emits an `update:revealed` signal the
// parent uses to gate Finish, and re-locks (clears reveal + confirm) whenever
// the phrase prop changes. These tests exercise that gate directly so a user can
// never attest "I saved my recovery phrase" while it is still hidden.

const PHRASE = ["alpha", "bravo", "charlie"];

function mountReveal(phrase: string[] = PHRASE) {
  return mount(RecoveryPhraseReveal, {
    props: { phrase, confirmed: false },
    global: { plugins: [i18n] },
  });
}

describe("RecoveryPhraseReveal reveal-gate (R3-P1-1)", () => {
  it("disables the acknowledge checkbox until the phrase is revealed", async () => {
    const wrapper = mountReveal();
    const ack = () => wrapper.get('[data-testid="phrase-ack"]');

    // Before any reveal, the checkbox is disabled and no reveal was emitted.
    expect(ack().attributes("disabled")).toBeDefined();
    expect(wrapper.emitted("update:revealed")).toBeUndefined();
    // The "reveal first" hint is shown while the checkbox is locked.
    expect(wrapper.text()).toContain(
      i18n.global.t("recoveryPhrase.revealFirstHint"),
    );

    // Reveal the phrase.
    const revealBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    expect(revealBtn).toBeTruthy();
    await revealBtn!.trigger("click");
    await flushPromises();

    // Now the checkbox is enabled and a `revealed=true` signal was emitted.
    expect(ack().attributes("disabled")).toBeUndefined();
    const revealedEvents = wrapper.emitted("update:revealed");
    expect(revealedEvents).toBeTruthy();
    expect(revealedEvents![0]).toEqual([true]);
  });

  it("emits update:confirmed only after reveal + check", async () => {
    const wrapper = mountReveal();
    const revealBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    await revealBtn!.trigger("click");
    await flushPromises();

    const ack = wrapper.get('[data-testid="phrase-ack"]');
    await ack.setValue(true);
    const confirmedEvents = wrapper.emitted("update:confirmed");
    expect(confirmedEvents).toBeTruthy();
    expect(confirmedEvents![confirmedEvents!.length - 1]).toEqual([true]);
  });

  it("re-locks (clears reveal + confirm) when the phrase changes", async () => {
    const wrapper = mountReveal();
    // Reveal + acknowledge the first phrase.
    const revealBtn = () =>
      wrapper
        .findAll("button")
        .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    await revealBtn()!.trigger("click");
    await flushPromises();
    await wrapper.get('[data-testid="phrase-ack"]').setValue(true);
    await wrapper.setProps({ confirmed: true });
    await flushPromises();

    // Change the phrase: the component must re-lock - emit revealed=false and
    // confirmed=false, and re-disable the checkbox until re-revealed.
    await wrapper.setProps({ phrase: ["delta", "echo", "foxtrot"] });
    await flushPromises();

    const revealedEvents = wrapper.emitted("update:revealed")!;
    expect(revealedEvents[revealedEvents.length - 1]).toEqual([false]);
    const confirmedEvents = wrapper.emitted("update:confirmed")!;
    expect(confirmedEvents[confirmedEvents.length - 1]).toEqual([false]);
    // The checkbox is locked again (a fresh phrase must be revealed anew).
    expect(
      wrapper.get('[data-testid="phrase-ack"]').attributes("disabled"),
    ).toBeDefined();

    // A button labelled "Reveal" is shown again (the phrase is hidden once more).
    expect(revealBtn()).toBeTruthy();
  });

  // M9c D4 (M6 R4-P1-1, DATA-SAFETY): when a backend reveal action is supplied,
  // the FIRST reveal must AWAIT it and only latch `everRevealed` on success - so
  // the backend records the reveal the ack gate requires. A rejected backend
  // reveal leaves the phrase hidden + the checkbox locked.
  it("awaits the backend reveal action and latches only on success", async () => {
    let calls = 0;
    const wrapper = mount(RecoveryPhraseReveal, {
      props: {
        phrase: PHRASE,
        confirmed: false,
        revealAction: async () => {
          calls += 1;
        },
      },
      global: { plugins: [i18n] },
    });
    const revealBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    await revealBtn!.trigger("click");
    await flushPromises();

    expect(calls).toBe(1);
    // Latched: the checkbox is enabled and revealed=true was emitted.
    expect(
      wrapper.get('[data-testid="phrase-ack"]').attributes("disabled"),
    ).toBeUndefined();
    expect(wrapper.emitted("update:revealed")![0]).toEqual([true]);
  });

  it("does not latch (or enable ack) when the backend reveal action rejects", async () => {
    const wrapper = mount(RecoveryPhraseReveal, {
      props: {
        phrase: PHRASE,
        confirmed: false,
        revealAction: async () => {
          throw { code: "crypto.key_missing" };
        },
      },
      global: { plugins: [i18n] },
    });
    const revealBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    await revealBtn!.trigger("click");
    await flushPromises();

    // The reveal failed: no revealed signal, the checkbox stays locked, and a
    // reveal-error was surfaced for the parent.
    expect(wrapper.emitted("update:revealed")).toBeUndefined();
    expect(
      wrapper.get('[data-testid="phrase-ack"]').attributes("disabled"),
    ).toBeDefined();
    expect(wrapper.emitted("reveal-error")).toBeTruthy();
    // The words are not shown (still hidden).
    expect(wrapper.find('[data-testid="phrase-words"]').exists()).toBe(false);
  });

  it("keeps the checkbox disabled with no phrase even after a toggle attempt", async () => {
    // An empty phrase (the unencrypted case) never enables the checkbox.
    const wrapper = mountReveal([]);
    const revealBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("recoveryPhrase.revealButton"));
    // The reveal button is disabled (no phrase), and toggling cannot mark it
    // revealed, so the checkbox stays disabled.
    await revealBtn!.trigger("click");
    await flushPromises();
    expect(
      wrapper.get('[data-testid="phrase-ack"]').attributes("disabled"),
    ).toBeDefined();
    expect(wrapper.emitted("update:revealed")).toBeUndefined();
  });
});
