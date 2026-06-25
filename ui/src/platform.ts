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

/** True when the current webview is running on macOS. Reads the live navigator
 * userAgent; falls back to false when navigator is unavailable (e.g. SSR/tests
 * that do not stub it). */
export function isMacOS(): boolean {
  if (typeof navigator === "undefined" || typeof navigator.userAgent !== "string") {
    return false;
  }
  return isMacUserAgent(navigator.userAgent);
}
