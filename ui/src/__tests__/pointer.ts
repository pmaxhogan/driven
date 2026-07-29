import { nextTick } from "vue";

// Shared pointer-event helper for the stat-tile hover tests.
//
// These tests cannot use `wrapper.trigger("pointermove", { clientX })`:
// @vue/test-utils constructs the event and then RE-ASSIGNS every option onto the
// instance, guarding only against *own* getter-only properties. jsdom implements
// `MouseEvent.clientX` as a spec-correct getter-only accessor on the PROTOTYPE,
// so from jsdom 26 on that re-assignment throws
// `TypeError: Cannot set property clientX of #<MouseEvent> which has only a getter`
// (@vue/test-utils#2591, still present in 2.4.11).
//
// Dispatching the event here instead passes the coordinate through the
// constructor's init dict - which is where a real browser puts it - and uses the
// `PointerEvent` interface the component's handler is actually typed against
// (@vue/test-utils' event-type table still maps `pointermove` to `MouseEvent`).

/** Dispatch a bubbling `pointermove` at `clientX` on `el`, then flush the DOM. */
export async function pointerMoveAt(el: Element, clientX: number): Promise<void> {
  el.dispatchEvent(new PointerEvent("pointermove", { clientX, bubbles: true, cancelable: true }));
  await nextTick();
}
