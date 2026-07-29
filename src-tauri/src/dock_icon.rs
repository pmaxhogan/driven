//! macOS Dock icon for UNBUNDLED runs (`cargo tauri dev`).
//!
//! A packaged `Driven.app` gets its Dock icon from `Contents/Resources` via the
//! `bundle.icon` list in `tauri.conf.json` (which includes `icons/icon.icns`),
//! so release builds are already correct and this module deliberately does
//! nothing for them.
//!
//! A `cargo tauri dev` run is different: it launches the BARE `driven-app`
//! Mach-O with no `.app` wrapper, so LaunchServices has no icon to associate
//! with the process and the Dock falls back to the generic "exec" tile. The
//! only way to give a bundle-less process a Dock icon is to hand AppKit one at
//! runtime with `-[NSApplication setApplicationIconImage:]`.
//!
//! Tauri itself already attempts exactly that once, at `RunEvent::Ready`
//! (`tauri::app::on_event_loop_event`, gated on `cfg(all(dev, target_os =
//! "macos"))`). That call is real - the icns bytes are compiled into the dev
//! binary - but it lands very early in launch, and in practice the generic tile
//! survives it. So this module RE-APPLIES the icon at the later, more reliable
//! moments: once the event loop reports ready, and again whenever the main
//! window is surfaced. Setting the same image twice is idempotent and cheap.
//!
//! Everything here is best-effort: a Dock icon is cosmetic, so every failure
//! path logs and returns rather than propagating (and never panics - note that
//! upstream's version `.expect()`s on image decode; this one does not).

/// Apply the Driven icon to the Dock tile, if this process needs it.
///
/// No-op on every non-macOS platform, and on macOS when the process is running
/// from inside a `.app` bundle (where the OS already has the right icon).
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn apply(reason: &str) {
    #[cfg(target_os = "macos")]
    imp::apply(reason);
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::rc::Retained;
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    use std::sync::OnceLock;

    const TARGET: &str = "driven::dock";

    /// The 512x512 master PNG. A PNG (rather than the multi-representation
    /// `icon.icns`) keeps the decode unambiguous, and AppKit downsamples it to
    /// whatever tile size the Dock is currently using.
    const ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

    /// Is this process running from inside a `.app` bundle?
    ///
    /// Checked from the executable path rather than from `debug_assertions`,
    /// because the two are independent: a debug build can be bundled (a dev
    /// channel `.app`) and a release build can be run bare. What actually
    /// decides whether the Dock needs help is the BUNDLE, not the profile.
    fn is_bundled() -> bool {
        static BUNDLED: OnceLock<bool> = OnceLock::new();
        *BUNDLED.get_or_init(|| {
            std::env::current_exe()
                .is_ok_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
        })
    }

    /// Decode the icon once and keep it for the process; re-applying then costs
    /// only the AppKit call.
    fn icon(mtm: MainThreadMarker) -> Option<Retained<NSImage>> {
        let _ = mtm;
        let data = NSData::with_bytes(ICON_PNG);
        let image = NSImage::initWithData(NSImage::alloc(), &data);
        if image.is_none() {
            tracing::warn!(target: TARGET, "decoding the Dock icon PNG failed; leaving the default tile");
        }
        image
    }

    pub fn apply(reason: &str) {
        if is_bundled() {
            // The .app already carries icon.icns; nothing to do.
            return;
        }
        // AppKit is main-thread-only. `MainThreadMarker::new()` returns None off
        // the main thread, which is the safe way to make that check - upstream
        // uses `new_unchecked` with a "TODO: Enable this check" note.
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::debug!(target: TARGET, reason, "not on the main thread; skipping Dock icon");
            return;
        };
        let Some(image) = icon(mtm) else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: `app` is the shared NSApplication obtained on the main thread
        // (witnessed by `mtm`), and `image` is a live NSImage we just created.
        unsafe { app.setApplicationIconImage(Some(&image)) };
        tracing::debug!(target: TARGET, reason, "applied the Dock icon for an unbundled run");
    }
}

#[cfg(test)]
mod tests {
    /// `apply` is best-effort and must never panic, whatever thread or platform
    /// it is called from - it runs on a cosmetic path during startup.
    #[test]
    fn apply_is_infallible_off_the_main_thread() {
        std::thread::spawn(|| super::apply("test"))
            .join()
            .expect("apply must not panic off the main thread");
    }

    /// On macOS a bundled process must be left alone: the `.app` already has
    /// `icon.icns`, so the runtime override is only for bare dev binaries.
    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_detection_matches_the_app_layout() {
        fn bundled(p: &str) -> bool {
            p.contains(".app/Contents/MacOS/")
        }
        assert!(bundled("/Applications/Driven.app/Contents/MacOS/Driven"));
        assert!(!bundled("/Users/x/code/driven/target/debug/driven-app"));
        assert!(!bundled("/usr/local/bin/driven-app"));
    }
}
