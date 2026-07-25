// Locale-aware byte formatting (DESIGN s8.7: never a hand-rolled English
// formatter). Extracted from Activity.vue so the header aggregates and the
// throughput tile's sparkline tooltip render identical strings for identical
// values - two formatters drifting apart is exactly how a chart ends up
// disagreeing with the number printed next to it.

/** Binary byte units, ascending. Index = the power of 1024 applied. */
const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;

/**
 * Format a byte count for display, e.g. `1536` -> `1.5 KB`.
 *
 * Non-positive, NaN and infinite inputs all render as `0 B`: this feeds stat
 * tiles and tooltips where a `NaN B` or `-3 KB` would be a visible bug, and
 * there is no meaningful negative byte count in this app.
 */
export function formatBytes(bytes: number, locale: string): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return `${new Intl.NumberFormat(locale).format(0)} ${BYTE_UNITS[0]}`;
  }
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), BYTE_UNITS.length - 1);
  const value = bytes / Math.pow(1024, exponent);
  const fmt = new Intl.NumberFormat(locale, {
    // Whole bytes read better without a decimal; everything above gets one.
    maximumFractionDigits: exponent === 0 ? 0 : 1,
  });
  return `${fmt.format(value)} ${BYTE_UNITS[exponent]}`;
}
