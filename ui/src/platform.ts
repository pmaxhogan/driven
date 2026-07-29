// Lightweight host-OS detection for the webview (ROADMAP M9 R1-P2-1).
//
// The in-app updater installs cleanly on Windows + Linux, but the V1 macOS
// updater path is not expected to work cleanly (DESIGN s15 / SPEC), so the
// About tab must NOT offer an in-app "Install update" on macOS - it shows a
// "Download the latest DMG" link to the GitHub release instead. We only need a
// coarse "is this macOS" check, which the WKWebView's userAgent reports
// reliably ("Macintosh" / "Mac OS X"); this avoids pulling in a whole new Tauri
// OS plugin + capability just for one boolean.
//
// Pure + injectable (the userAgent string is a parameter) so it is unit-tested
// without a real navigator.

/** True when `ua` is a macOS user-agent string. */
export function isMacUserAgent(ua: string): boolean {
  return /Macintosh|Mac OS X/i.test(ua);
}

/** True when `ua` is a Windows user-agent string.
 *
 * Used to hide settings that only DO anything on Windows - currently the
 * OneDrive / cloud-only placeholder policy, whose own caption admitted it
 * "applies to ... on Windows (harmless elsewhere)". "Harmless elsewhere" is a
 * note for the implementer; a control that provably does nothing on the user's
 * platform should not be on their screen. */
export function isWindowsUserAgent(ua: string): boolean {
  return /Windows/i.test(ua);
}

/** True when the current webview is running on macOS. Reads the live navigator
 * userAgent; falls back to false when navigator is unavailable (e.g. SSR/tests
 * that do not stub it). */
export function isMacOS(): boolean {
  if (typeof navigator === "undefined" || typeof navigator.userAgent !== "string") {
    return false;
  }
  return isMacUserAgent(navigator.userAgent);
}

/** True when the current webview is running on Windows. Same shape (and same
 * no-navigator fallback) as [`isMacOS`]. Falling back to FALSE is the safe
 * direction here: it hides a Windows-only control rather than showing an inert
 * one, and the value it would have edited already defaults correctly. */
export function isWindows(): boolean {
  if (typeof navigator === "undefined" || typeof navigator.userAgent !== "string") {
    return false;
  }
  return isWindowsUserAgent(navigator.userAgent);
}
