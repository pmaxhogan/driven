// R1-P2-3: localize an activity-log event type into a human label, instead of
// rendering the raw backend code. DESIGN wants stable codes localized in the
// frontend. Extracted as a pure function (taking the i18n `t` + `te` seams) so
// it is unit-testable independent of the Activity.vue component.

/** vue-i18n's `t` (translate) shape this helper needs. */
export type TranslateFn = (key: string) => string;
/** vue-i18n's `te` (translation-exists) shape this helper needs. */
export type TranslationExistsFn = (key: string) => boolean;

/**
 * Resolve a localized label for an activity event type. Lookup chain, in order:
 *   1. `activity.events.<eventType>` - curated activity-event labels
 *      (upload_done, trash_done, scan_done, paused, the local.* warns).
 *   2. `errors.<eventType>.short` - the shared error/skip code labels (a
 *      Failed / Skipped row carries a SPEC s24 error code as its event type).
 *   3. The raw code itself - a SAFE fallback for an unknown / forward-compatible
 *      type so the cell never blanks or throws.
 */
export function activityEventLabel(
  eventType: string,
  t: TranslateFn,
  te: TranslationExistsFn
): string {
  const eventKey = `activity.events.${eventType}`;
  if (te(eventKey)) return t(eventKey);
  const errorKey = `errors.${eventType}.short`;
  if (te(errorKey)) return t(errorKey);
  return eventType;
}
