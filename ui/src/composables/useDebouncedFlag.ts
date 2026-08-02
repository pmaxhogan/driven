import { onScopeDispose, ref, watch, type Ref } from "vue";

// Smoke fix (2026-08-01 pause-banner branch): the orchestrator's periodic
// tick briefly transitions a PAUSED account through `PowerCheck` before
// re-pausing it (crates/driven-core/src/orchestrator.rs:2137, :2156-2159),
// and every transition is broadcast unconditionally with no dedup
// (orchestrator.rs:908-913) - so `GlobalProgressBar`'s `active` flag flips
// true for tens of milliseconds on every tick while paused, flashing the bar
// in and out. This composable is the generic fix: hysteresis on a boolean
// signal so a transition shorter than `delayMs` never surfaces.

/**
 * Debounces a boolean `source` on BOTH edges: the returned ref only takes on
 * a new value once `source` has held it continuously for `delayMs`. A blip
 * shorter than `delayMs` in either direction never surfaces - the returned
 * ref simply never changes for it.
 *
 * One re-armable timer (not a pair) does the work: any change to `source`
 * cancels whatever timer is pending and starts a fresh one, so a value that
 * reverses before the delay elapses leaves nothing running.
 *
 * The initial value is read straight from `source` with NO delay - this
 * debounces TRANSITIONS, not the starting state (a run already active when
 * the component mounts must show immediately, not after an artificial
 * wait).
 */
export function useDebouncedFlag(source: Ref<boolean>, delayMs: number): Ref<boolean> {
  const debounced = ref(source.value);
  let timer: ReturnType<typeof setTimeout> | null = null;

  function clearPending(): void {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  }

  watch(source, (value) => {
    clearPending();
    if (value === debounced.value) return;
    timer = setTimeout(() => {
      timer = null;
      debounced.value = value;
    }, delayMs);
  });

  // `onScopeDispose` (not `onBeforeUnmount`) so this also cleans up when used
  // outside a component - inside a manual `effectScope` (as the unit tests
  // do) or a plain composable composed into a bigger one.
  onScopeDispose(clearPending);

  return debounced;
}
