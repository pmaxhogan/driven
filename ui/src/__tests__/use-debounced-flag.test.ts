// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { effectScope, ref, type Ref } from "vue";

import { useDebouncedFlag } from "../composables/useDebouncedFlag";

// Pure temporal behaviour of useDebouncedFlag, run inside a manual
// `effectScope` (no component needed) so `onScopeDispose` and `watch` behave
// exactly as they would inside a real component's setup - `scope.stop()`
// stands in for unmount.

function run(source: Ref<boolean>, delayMs = 500) {
  const scope = effectScope();
  let debounced!: Ref<boolean>;
  scope.run(() => {
    debounced = useDebouncedFlag(source, delayMs);
  });
  return { debounced, stop: () => scope.stop() };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useDebouncedFlag", () => {
  it("takes the source's initial value immediately, with no delay", () => {
    const source = ref(true);
    const { debounced } = run(source);
    expect(debounced.value).toBe(true);
  });

  it("does not flip until the new value has held for the full delay", async () => {
    const source = ref(false);
    const { debounced } = run(source);

    source.value = true;
    await vi.advanceTimersByTimeAsync(499);
    expect(debounced.value).toBe(false);

    await vi.advanceTimersByTimeAsync(1);
    expect(debounced.value).toBe(true);
  });

  it("never surfaces a blip shorter than the delay (show edge)", async () => {
    const source = ref(false);
    const { debounced } = run(source);

    source.value = true;
    await vi.advanceTimersByTimeAsync(200);
    expect(debounced.value).toBe(false);

    source.value = false;
    await vi.advanceTimersByTimeAsync(1_000);
    expect(debounced.value).toBe(false);
  });

  it("never surfaces a blip shorter than the delay (hide edge)", async () => {
    const source = ref(true);
    const { debounced } = run(source);

    source.value = false;
    await vi.advanceTimersByTimeAsync(200);
    expect(debounced.value).toBe(true);

    source.value = true;
    await vi.advanceTimersByTimeAsync(1_000);
    expect(debounced.value).toBe(true);
  });

  it("commits a hide once the source holds false for the full delay", async () => {
    const source = ref(true);
    const { debounced } = run(source);

    source.value = false;
    await vi.advanceTimersByTimeAsync(500);
    expect(debounced.value).toBe(false);
  });

  it("clears its pending timer on scope dispose (no leaked timer)", async () => {
    const source = ref(false);
    const { debounced, stop } = run(source);

    source.value = true;
    stop();
    await vi.advanceTimersByTimeAsync(1_000);
    // The scope was stopped before the delay elapsed - the flip must not
    // still land (nothing should be listening, but if a stray timer fired
    // and threw or wrote to a disposed watcher this would surface).
    expect(debounced.value).toBe(false);
  });
});
